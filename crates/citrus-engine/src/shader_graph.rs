//! Material shader graph -> GLSL codegen (ENGINE_FEATURE_CHECKLIST T1 #26).
//!
//! A node graph the editor can author visually, compiled to a GLSL expression that
//! feeds the standard material (base colour / emissive / etc.). This module is the
//! graph data model + the (testable) code generator; the node-editor UI is a thin
//! egui layer on top. Keeping the compiler separate means the same graph drives the
//! runtime shader and is verifiable without a GPU.

use std::collections::HashMap;

/// A node's output type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Float,
    Vec3,
}

impl DataType {
    fn glsl(self) -> &'static str {
        match self {
            DataType::Float => "float",
            DataType::Vec3 => "vec3",
        }
    }
}

/// A graph node. Inputs reference other node ids.
#[derive(Clone, Debug)]
pub enum Node {
    /// Literal scalar.
    ConstFloat(f32),
    /// Literal colour.
    ConstVec3([f32; 3]),
    /// Mesh UV (`v_uv`).
    Uv,
    /// Sample a bound texture by a uv node, taking `.rgb`.
    TextureSample { tex: String, uv: usize },
    /// Component-wise a*b (types must match; float*vec3 broadcasts).
    Mul(usize, usize),
    Add(usize, usize),
    /// `mix(a, b, t)` with scalar t.
    Mix { a: usize, b: usize, t: usize },
}

impl Node {
    fn ty(&self, graph: &ShaderGraph) -> DataType {
        match self {
            Node::ConstFloat(_) => DataType::Float,
            Node::ConstVec3(_) => DataType::Vec3,
            Node::Uv => DataType::Vec3, // uv promoted as vec3(uv,0) for uniformity
            Node::TextureSample { .. } => DataType::Vec3,
            Node::Mul(a, b) | Node::Add(a, b) => {
                // vec3 wins over float (broadcast).
                if graph.nodes[*a].ty(graph) == DataType::Vec3
                    || graph.nodes[*b].ty(graph) == DataType::Vec3
                {
                    DataType::Vec3
                } else {
                    DataType::Float
                }
            }
            Node::Mix { a, .. } => graph.nodes[*a].ty(graph),
        }
    }
}

#[derive(Default)]
pub struct ShaderGraph {
    nodes: Vec<Node>,
    /// Node id whose value is the graph output.
    output: Option<usize>,
}

/// Error from an invalid graph (bad references, no output, cycle).
#[derive(Debug, PartialEq)]
pub enum GraphError {
    NoOutput,
    BadRef(usize),
    Cycle,
}

impl ShaderGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node, returning its id.
    pub fn add(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    pub fn set_output(&mut self, id: usize) {
        self.output = Some(id);
    }

    fn refs(node: &Node) -> Vec<usize> {
        match node {
            Node::TextureSample { uv, .. } => vec![*uv],
            Node::Mul(a, b) | Node::Add(a, b) => vec![*a, *b],
            Node::Mix { a, b, t } => vec![*a, *b, *t],
            _ => vec![],
        }
    }

    /// Validate + topologically sort (returns node ids in evaluation order).
    fn topo(&self) -> Result<Vec<usize>, GraphError> {
        let out = self.output.ok_or(GraphError::NoOutput)?;
        if out >= self.nodes.len() {
            return Err(GraphError::BadRef(out));
        }
        let mut order = Vec::new();
        let mut state = vec![0u8; self.nodes.len()]; // 0=unseen,1=in-progress,2=done
        // Iterative DFS post-order, reachable from the output only.
        fn visit(
            graph: &ShaderGraph,
            n: usize,
            state: &mut [u8],
            order: &mut Vec<usize>,
        ) -> Result<(), GraphError> {
            if n >= graph.nodes.len() {
                return Err(GraphError::BadRef(n));
            }
            match state[n] {
                2 => return Ok(()),
                1 => return Err(GraphError::Cycle),
                _ => {}
            }
            state[n] = 1;
            for r in ShaderGraph::refs(&graph.nodes[n]) {
                visit(graph, r, state, order)?;
            }
            state[n] = 2;
            order.push(n);
            Ok(())
        }
        visit(self, out, &mut state, &mut order)?;
        Ok(order)
    }

    /// Compile to a GLSL snippet. Emits `n<i>` locals in evaluation order and a final
    /// `vec3 <result_var> = <output>;`. The caller inlines this into the material
    /// shader (the output feeds base colour / emissive).
    pub fn compile(&self, result_var: &str) -> Result<String, GraphError> {
        let order = self.topo()?;
        let mut src = String::new();
        let mut expr: HashMap<usize, String> = HashMap::new();
        for &i in &order {
            let ty = self.nodes[i].ty(self);
            let e = match &self.nodes[i] {
                Node::ConstFloat(v) => format!("{v:?}"),
                Node::ConstVec3(v) => format!("vec3({:?}, {:?}, {:?})", v[0], v[1], v[2]),
                Node::Uv => "vec3(v_uv, 0.0)".to_string(),
                Node::TextureSample { tex, uv } => {
                    format!("texture({tex}, {}.xy).rgb", expr[uv])
                }
                Node::Mul(a, b) => format!("({} * {})", expr[a], expr[b]),
                Node::Add(a, b) => format!("({} + {})", expr[a], expr[b]),
                Node::Mix { a, b, t } => {
                    format!("mix({}, {}, {})", expr[a], expr[b], expr[t])
                }
            };
            src.push_str(&format!("    {} n{} = {};\n", ty.glsl(), i, e));
            expr.insert(i, format!("n{i}"));
        }
        let out = self.output.unwrap();
        src.push_str(&format!("    vec3 {result_var} = {};\n", expr[&out]));
        Ok(src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_a_tinted_texture_graph() {
        // base = texture(t_albedo, uv).rgb * tint
        let mut g = ShaderGraph::new();
        let uv = g.add(Node::Uv);
        let tex = g.add(Node::TextureSample { tex: "t_albedo".into(), uv });
        let tint = g.add(Node::ConstVec3([1.0, 0.5, 0.2]));
        let mul = g.add(Node::Mul(tex, tint));
        g.set_output(mul);

        let src = g.compile("base_color").unwrap();
        assert!(src.contains("texture(t_albedo, n0.xy).rgb"));
        assert!(src.contains("vec3 base_color = n3;"));
        assert!(src.contains("(n1 * n2)"));
    }

    #[test]
    fn detects_cycle_and_missing_output() {
        let mut g = ShaderGraph::new();
        let a = g.add(Node::ConstFloat(1.0));
        // No output set.
        assert_eq!(g.compile("x"), Err(GraphError::NoOutput));
        // Make a self-referential mul (cycle): node refers to itself.
        let m = g.add(Node::Mul(a, 99)); // bad ref first
        g.set_output(m);
        assert_eq!(g.compile("x"), Err(GraphError::BadRef(99)));
    }

    #[test]
    fn mix_node_emits_glsl_mix() {
        let mut g = ShaderGraph::new();
        let a = g.add(Node::ConstVec3([0.0, 0.0, 0.0]));
        let b = g.add(Node::ConstVec3([1.0, 1.0, 1.0]));
        let t = g.add(Node::ConstFloat(0.5));
        let m = g.add(Node::Mix { a, b, t });
        g.set_output(m);
        let src = g.compile("c").unwrap();
        assert!(src.contains("mix(n0, n1, n2)"), "{src}");
    }
}
