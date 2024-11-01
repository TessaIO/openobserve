// Copyright 2024 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use config::{
    meta::{
        function::{Transform, VRLResultResolver},
        pipeline::{components::NodeData, Pipeline},
        stream::{StreamParams, StreamType},
    },
    utils::{flatten, json::Value},
};
use futures::future::try_join_all;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use crate::{
    common::infra::config::QUERY_FUNCTIONS,
    service::ingestion::{apply_vrl_fn, compile_vrl_function},
};

#[async_trait]
pub trait PipelineExt: Sync + Send + 'static {
    /// Registers the function of all the FunctionNode of this pipeline once for execution.
    /// Returns a map of node_id -> VRLResultResolver for quick lookup
    async fn register_functions(&self) -> Result<HashMap<String, VRLResultResolver>>;
}

#[async_trait]
impl PipelineExt for Pipeline {
    async fn register_functions(&self) -> Result<HashMap<String, VRLResultResolver>> {
        let mut vrl_map = HashMap::new();
        for node in &self.nodes {
            if let NodeData::Function(func_params) = &node.data {
                let transform = get_transforms(&self.org, &func_params.name).await?;
                if let Ok(vrl_runtime_config) = compile_vrl_function(&transform.function, &self.org)
                {
                    let registry = vrl_runtime_config
                        .config
                        .get_custom::<vector_enrichment::TableRegistry>()
                        .unwrap();
                    registry.finish_load();
                    vrl_map.insert(
                        node.get_node_id(),
                        VRLResultResolver {
                            program: vrl_runtime_config.program,
                            fields: vrl_runtime_config.fields,
                        },
                    );
                }
            }
        }
        Ok(vrl_map)
    }
}

#[derive(Debug, Clone)]
pub struct ExecutablePipeline {
    source_node_id: String,
    sorted_nodes: Vec<String>,
    vrl_map: HashMap<String, VRLResultResolver>,
    node_map: HashMap<String, ExecutableNode>,
}

#[derive(Debug, Clone)]
pub struct ExecutableNode {
    node_data: NodeData,
    children: Vec<String>,
}

#[derive(Debug)]
pub struct ExecutablePipelineBulkInputs {
    records: Vec<Value>,
    doc_ids: Vec<Option<String>>,
    originals: Vec<Option<String>>,
}

#[derive(Debug)]
pub struct ExecutablePipelineTraceInputs {
    records: Vec<Value>,
    timestamps: Vec<i64>,
    services: Vec<String>,
    span_names: Vec<String>,
    span_status_for_spanmetrics: Vec<String>,
    span_kinds: Vec<String>,
    span_durations: Vec<f64>,
}

impl ExecutablePipeline {
    pub async fn new(pipeline: &Pipeline) -> Result<Self> {
        let node_map = pipeline
            .nodes
            .iter()
            .map(|node| {
                (
                    node.get_node_id(),
                    ExecutableNode {
                        node_data: node.get_node_data(),
                        children: pipeline
                            .edges
                            .iter()
                            .filter(|edge| edge.source == node.id)
                            .map(|edge| edge.target.clone())
                            .collect(),
                    },
                )
            })
            .collect();

        let vrl_map = pipeline.register_functions().await?;
        let sorted_nodes = topological_sort(&node_map)?;
        let source_node_id = sorted_nodes[0].to_owned();

        Ok(Self {
            source_node_id,
            node_map,
            sorted_nodes,
            vrl_map,
        })
    }

    pub async fn process_batch(
        &self,
        org_id: &str,
        records: Vec<Value>,
    ) -> Result<HashMap<StreamParams, Vec<(usize, Value)>>> {
        let batch_size = records.len();
        log::debug!("[Pipeline]: process batch of size {}", batch_size);

        let (result_sender, mut result_receiver) =
            channel::<(usize, StreamParams, Value)>(batch_size);

        let mut node_senders = HashMap::new();
        let mut node_receivers = HashMap::new();

        for node_id in &self.sorted_nodes {
            let (sender, receiver) = channel::<(usize, Value, bool)>(batch_size);
            node_senders.insert(node_id.to_string(), sender);
            node_receivers.insert(node_id.to_string(), receiver);
        }

        // Spawn tasks for each node
        let mut node_tasks = Vec::new();
        for (idx, node_id) in self.sorted_nodes.iter().enumerate() {
            let org_id_cp = org_id.to_string();
            let node = self.node_map.get(node_id).unwrap().clone();
            let node_receiver = node_receivers.remove(node_id).unwrap();
            let child_senders: Vec<_> = node
                .children
                .iter()
                .map(|child| node_senders.get(child).unwrap().clone())
                .collect();
            let result_sender_cp = node.children.is_empty().then_some(result_sender.clone());
            let vrl_runtime = self.vrl_map.get(node_id).cloned();

            let task = tokio::spawn(async move {
                process_node(
                    idx,
                    org_id_cp,
                    node,
                    node_receiver,
                    child_senders,
                    vrl_runtime,
                    result_sender_cp,
                )
                .await
            });
            node_tasks.push(task);
        }

        let result_task = tokio::spawn(async move {
            // Collect results
            log::debug!("[Pipeline]: starts result collecting job");
            let mut count: usize = 0;
            let mut results = HashMap::new();
            while let Some((idx, stream_params, record)) = result_receiver.recv().await {
                results
                    .entry(stream_params)
                    .or_insert(Vec::new())
                    .push((idx, record));
                count += 1;
            }
            log::debug!("[Pipeline]: collected {count} records");
            results
        });

        // Send records to the source node to begin processing
        let flattened = {
            let source_node = self.node_map.get(&self.source_node_id).unwrap();
            matches!(&source_node.node_data, NodeData::Stream(stream_params) if stream_params.stream_type == StreamType::Metrics)
        };
        let source_sender = node_senders.remove(&self.source_node_id).unwrap();
        for (idx, record) in records.into_iter().enumerate() {
            if let Err(send_err) = source_sender.send((idx, record, flattened)).await {
                log::error!(
                    "[Pipeline]: Error sending original records into source Node for {send_err}"
                );
                break;
            }
        }
        drop(source_sender);
        drop(result_sender);
        drop(node_senders);
        log::debug!("[Pipeline]: All records send into pipeline for processing");

        // Wait for all node tasks to complete
        if let Err(e) = try_join_all(node_tasks).await {
            log::error!("[Pipeline] node processing jobs failed: {}", e);
        }

        let results = result_task.await.map_err(|e| {
            log::error!("[Pipeline] result collecting job failed: {}", e);
            anyhow!("[Pipeline] result collecting job failed: {}", e)
        })?;

        Ok(results)
    }

    pub fn get_all_destination_streams(&self) -> Vec<StreamParams> {
        self.node_map
            .values()
            .filter_map(|exec_node| {
                if exec_node.children.is_empty() {
                    if let NodeData::Stream(stream_params) = &exec_node.node_data {
                        Some(stream_params.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn num_of_func(&self) -> usize {
        self.node_map
            .values()
            .filter(|exec_node| matches!(exec_node.node_data, NodeData::Function(_)))
            .count()
    }
}

impl ExecutablePipelineBulkInputs {
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            doc_ids: Vec::new(),
            originals: Vec::new(),
        }
    }

    pub fn add_input(
        &mut self,
        record: Value,
        doc_id: Option<String>,
        original_data: Option<String>,
    ) {
        self.records.push(record);
        self.doc_ids.push(doc_id);
        self.originals.push(original_data);
    }

    pub fn into_parts(self) -> (Vec<Value>, Vec<Option<String>>, Vec<Option<String>>) {
        (self.records, self.doc_ids, self.originals)
    }
}

impl Default for ExecutablePipelineBulkInputs {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutablePipelineTraceInputs {
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            timestamps: Vec::new(),
            services: Vec::new(),
            span_names: Vec::new(),
            span_status_for_spanmetrics: Vec::new(),
            span_kinds: Vec::new(),
            span_durations: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_input(
        &mut self,
        record: Value,
        ts: i64,
        service: String,
        span_name: String,
        span_status_for_spanmetric: String,
        span_kind: String,
        duration: f64,
    ) {
        self.records.push(record);
        self.timestamps.push(ts);
        self.services.push(service);
        self.span_names.push(span_name);
        self.span_status_for_spanmetrics
            .push(span_status_for_spanmetric);
        self.span_kinds.push(span_kind);
        self.span_durations.push(duration);
    }

    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        Vec<Value>,
        Vec<i64>,
        Vec<String>,
        Vec<String>,
        Vec<String>,
        Vec<String>,
        Vec<f64>,
    ) {
        (
            self.records,
            self.timestamps,
            self.services,
            self.span_names,
            self.span_status_for_spanmetrics,
            self.span_kinds,
            self.span_durations,
        )
    }
}

impl Default for ExecutablePipelineTraceInputs {
    fn default() -> Self {
        Self::new()
    }
}

async fn process_node(
    node_id: usize,
    org_id: String,
    node: ExecutableNode,
    mut receiver: Receiver<(usize, Value, bool)>,
    mut child_senders: Vec<Sender<(usize, Value, bool)>>,
    vrl_runtime: Option<VRLResultResolver>,
    result_sender: Option<Sender<(usize, StreamParams, Value)>>,
) -> Result<()> {
    let cfg = config::get_config();
    let mut count: usize = 0;
    match &node.node_data {
        NodeData::Stream(stream_params) => {
            if node.children.is_empty() {
                log::debug!("[Pipeline]: Leaf node {node_id} starts processing");
                // leaf node: `result_sender` guaranteed to be Some()
                // send received results directly via `result_sender` for collection
                let result_sender = result_sender.unwrap();
                while let Some((idx, mut record, flattened)) = receiver.recv().await {
                    if !flattened {
                        record =
                            flatten::flatten_with_level(record, cfg.limit.ingest_flatten_level)
                                .map_err(|e| anyhow!("LeafNode error with flattening: {}", e))?;
                    }
                    if let Err(send_err) = result_sender
                        .send((idx, stream_params.clone(), record))
                        .await
                    {
                        log::error!(
                            "[Pipeline]: Leaf node errors sending result for collection caused by: {send_err}"
                        );
                        break;
                    }
                    count += 1;
                }
                log::debug!("[Pipeline]: leaf node {node_id} done processing {count} records");
            } else {
                log::debug!("[Pipeline]: source node {node_id} starts processing");
                // source stream node: send received record to all its children
                while let Some(item) = receiver.recv().await {
                    send_to_children(&mut child_senders, item, "StreamNode").await;
                    count += 1;
                }
                log::debug!("[Pipeline]: source node {node_id} done processing {count} records");
            }
        }
        NodeData::Condition(condition_params) => {
            log::debug!("[Pipeline]: cond node {node_id} starts processing");
            while let Some((idx, mut record, mut flattened)) = receiver.recv().await {
                // value must be flattened before condition params can take effect
                if !flattened {
                    record = flatten::flatten_with_level(record, cfg.limit.ingest_flatten_level)
                        .map_err(|e| anyhow!("ConditionNode error with flattening: {}", e))?;
                    flattened = true;
                }
                // only send to children when passing all condition evaluations
                if condition_params
                    .conditions
                    .iter()
                    .all(|cond| cond.evaluate(record.as_object().unwrap()))
                {
                    send_to_children(
                        &mut child_senders,
                        (idx, record, flattened),
                        "ConditionNode",
                    )
                    .await;
                    count += 1;
                }
            }
            log::debug!("[Pipeline]: cond node {node_id} done processing {count} records");
        }
        NodeData::Function(func_params) => {
            log::debug!("[Pipeline]: func node {node_id} starts processing");
            let mut runtime = crate::service::ingestion::init_functions_runtime();
            while let Some((idx, mut record, mut flattened)) = receiver.recv().await {
                if let Some(vrl_runtime) = &vrl_runtime {
                    if func_params.after_flatten && !flattened {
                        record =
                            flatten::flatten_with_level(record, cfg.limit.ingest_flatten_level)
                                .map_err(|e| {
                                    anyhow!("FunctionNode error with flattening: {}", e)
                                })?;
                    }
                    record = apply_vrl_fn(
                        &mut runtime,
                        vrl_runtime,
                        record,
                        &org_id,
                        &["pipeline".to_string()],
                    );
                    flattened = false; // since apply_vrl_fn can produce unflattened data
                }
                send_to_children(&mut child_senders, (idx, record, flattened), "FunctionNode")
                    .await;
                count += 1;
            }
            log::debug!("[Pipeline]: func node {node_id} done processing {count} records");
        }
        NodeData::Query(_) => {
            // source node for Scheduled pipeline. Directly send to children nodes
            log::debug!("[Pipeline]: query node {node_id} starts processing");
            while let Some(item) = receiver.recv().await {
                send_to_children(&mut child_senders, item, "QueryNode").await;
                count += 1;
            }
            log::debug!("[Pipeline]: query node {node_id} done processing {count} records");
        }
    }

    // all cloned senders dropped when function goes out of scope -> close the channel

    Ok(())
}

async fn send_to_children(
    child_senders: &mut [Sender<(usize, Value, bool)>],
    item: (usize, Value, bool),
    node_type: &str,
) {
    if child_senders.len() == 1 {
        // HACK to avoid cloning
        if let Err(send_err) = child_senders[0].send(item).await {
            log::error!(
                "[Pipeline]: {node_type} errors sending record to its children caused by: {send_err}"
            );
        }
    } else {
        for child_sender in child_senders.iter_mut() {
            if let Err(send_err) = child_sender.send(item.clone()).await {
                log::error!(
                    "[Pipeline]: {node_type} errors sending record to its children caused by: {send_err}"
                );
                break;
            }
        }
    }
}

fn topological_sort(node_map: &HashMap<String, ExecutableNode>) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut temp = HashSet::new();

    let graph: HashMap<String, Vec<String>> = node_map
        .iter()
        .map(|(k, v)| (k.clone(), v.children.clone()))
        .collect();

    for node in node_map.keys() {
        if let Err(e) = dfs(node, &graph, &mut visited, &mut temp, &mut result) {
            return Err(anyhow!(
                "[Pipeline] Error to topological sort the nodes in preparing execution: {}",
                e
            ));
        }
    }

    result.reverse();
    Ok(result)
}

fn dfs(
    current_node_id: &str,
    graph: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
    temp: &mut HashSet<String>,
    result: &mut Vec<String>,
) -> Result<()> {
    if temp.contains(current_node_id) {
        return Err(anyhow!("Cyclical pipeline detected."));
    }
    if visited.contains(current_node_id) {
        return Ok(());
    }
    temp.insert(current_node_id.to_string());
    if let Some(neighbors) = graph.get(current_node_id) {
        for neighbor in neighbors {
            dfs(neighbor, graph, visited, temp, result)?;
        }
    }
    temp.remove(current_node_id);
    visited.insert(current_node_id.to_string());
    result.push(current_node_id.to_string());
    Ok(())
}

async fn get_transforms(org_id: &str, fn_name: &str) -> Result<Transform> {
    let func_key = format!("{org_id}/{fn_name}");
    if let Some(trans) = QUERY_FUNCTIONS.get(&func_key) {
        return Ok(trans.value().clone());
    }
    // get from database
    crate::service::db::functions::get(org_id, fn_name).await
}
