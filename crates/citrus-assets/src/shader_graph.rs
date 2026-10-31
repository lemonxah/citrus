//! Visual shader graph: a node DAG compiled to GLSL. Each node computes a
//! scalar from other nodes; the compiler topologically orders the graph and
//! emits one GLSL expression per node.

use serde::{Deserialize, Serialize};

/// A shader-graph node: a scalar value or an operation over other nodes
/// (referenced by id).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ShaderNode {
    Value { value: f32 },
    Property { name: String },
    Add { a: usize, b: usize },
    Mul { a: usize, b: usize },
    Mix { a: usize, b: usize, t: usize },
    Sin { x: usize },
    Clamp { x: usize, lo: usize, hi: usize },
}

/// A shader graph: `nodes[i]` is node `i` (`None` = an absent id).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShaderGraph {
    pub nodes: Vec<Option<ShaderNode>>,
}

fn inputs(node: &ShaderNode) -> Vec<usize> {
    match node {
        ShaderNode::Value { .. } | ShaderNode::Property { .. } => Vec::new(),
        ShaderNode::Add { a, b } => vec![*a, *b],
        ShaderNode::Mul { a, b } => vec![*a, *b],
        ShaderNode::Mix { a, b, t } => vec![*a, *b, *t],
        ShaderNode::Sin { x } => vec![*x],
        ShaderNode::Clamp { x, lo, hi } => vec![*x, *lo, *hi],
    }
}

/// Topologically order the nodes (dependencies first). Errors on a cycle or a
/// reference to an absent/out-of-range node.
pub fn topo_order(graph: &ShaderGraph) -> Result<Vec<usize>, String> {
    let n = graph.nodes.len();
    let mut indegree = vec![0usize; n];
    let mut adj = vec![Vec::new(); n];
    for (id, node) in graph.nodes.iter().enumerate() {
        let Some(node) = node else { continue };
        for &dep in &inputs(node) {
            if dep >= n || graph.nodes[dep].is_none() {
                return Err(format!("node {id} references missing node {dep}"));
            }
            indegree[id] += 1;
            adj[dep].push(id);
        }
    }
    let mut queue: Vec<usize> = (0..n).filter(|&i| indegree[i] == 0 && graph.nodes[i].is_some()).collect();
    let mut out = Vec::with_capacity(n);
    while let Some(id) = queue.pop() {
        out.push(id);
        for &next in &adj[id] {
            indegree[next] -= 1;
            if indegree[next] == 0 {
                queue.push(next);
            }
        }
    }
    if out.len() != graph.nodes.iter().filter(|n| n.is_some()).count() {
        return Err("graph contains a cycle".into());
    }
    Ok(out)
}

/// Emit one GLSL statement per node (in dependency order), where node `id`'s
/// value is `v{id}`.
pub fn generate_glsl(graph: &ShaderGraph, order: &[usize]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(order.len());
    for &id in order {
        let node = graph.nodes.get(id).and_then(|n| n.as_ref()).ok_or_else(|| format!("missing node {id}"))?;
        let line = match node {
            ShaderNode::Value { value } => format!("float v{id} = {value:?};"),
            ShaderNode::Property { name } => format!("float v{id} = u_{name};"),
            ShaderNode::Add { a, b } => format!("float v{id} = v{a} + v{b};"),
            ShaderNode::Mul { a, b } => format!("float v{id} = v{a} * v{b};"),
            ShaderNode::Mix { a, b, t } => format!("float v{id} = mix(v{a}, v{b}, v{t});"),
            ShaderNode::Sin { x } => format!("float v{id} = sin(v{x});"),
            ShaderNode::Clamp { x, lo, hi } => format!("float v{id} = clamp(v{x}, v{lo}, v{hi});"),
        };
        out.push(line);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(order: &[usize], id: usize) -> usize {
        order.iter().position(|&i| i == id).unwrap()
    }

    #[test]
    fn topo_order_puts_dependencies_first() {
        // 0: Value(0.5) -> 1: Add(0,2) ... a diamond: 0,1 values, 2 add, 3 mul.
        let g = ShaderGraph {
            nodes: vec![
                Some(ShaderNode::Value { value: 0.5 }),
                Some(ShaderNode::Value { value: 2.0 }),
                Some(ShaderNode::Add { a: 0, b: 1 }),
                Some(ShaderNode::Mul { a: 2, b: 2 }),
            ],
        };
        let order = topo_order(&g).unwrap();
        assert!(pos(&order, 0) < pos(&order, 2));
        assert!(pos(&order, 1) < pos(&order, 2));
        assert!(pos(&order, 2) < pos(&order, 3));
    }

    #[test]
    fn topo_order_detects_cycles() {
        let g = ShaderGraph {
            nodes: vec![
                Some(ShaderNode::Add { a: 1, b: 1 }),
                Some(ShaderNode::Add { a: 0, b: 0 }),
            ],
        };
        assert!(topo_order(&g).is_err());
    }

    #[test]
    fn topo_order_rejects_missing_nodes() {
        let g = ShaderGraph { nodes: vec![Some(ShaderNode::Add { a: 0, b: 7 })] };
        assert!(topo_order(&g).is_err());
    }

    #[test]
    fn generate_glsl_emits_dependency_order() {
        let g = ShaderGraph {
            nodes: vec![
                Some(ShaderNode::Value { value: 0.5 }),
                Some(ShaderNode::Value { value: 2.0 }),
                Some(ShaderNode::Add { a: 0, b: 1 }),
                Some(ShaderNode::Mul { a: 2, b: 2 }),
            ],
        };
        let order = topo_order(&g).unwrap();
        let glsl = generate_glsl(&g, &order).unwrap();
        assert_eq!(glsl.len(), 4);
        let line = |id: usize| glsl.iter().position(|l| l.starts_with(&format!("float v{id} ="))).unwrap();
        assert!(line(0) < line(2) && line(1) < line(2));
        assert!(line(2) < line(3));
        assert!(glsl[line(2)].contains("v0 + v1"));
        assert!(glsl[line(3)].contains("v2 * v2"));
    }
}
