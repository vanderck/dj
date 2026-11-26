//! A Demucs model in 16-bit floats, for the Neural Engine.
//!
//! CoreML runs a 32-bit model on the CPU even when it may use the Neural Engine, which computes
//! only in 16 bits. [`half_precision`] rewrites an ONNX model (one ONNX Runtime has constant-folded:
//! see `onnx::half_model`) so that the network — encoders, decoders, transformer — is 16-bit and
//! the signal processing around it stays 32-bit: the input's normalisation, the STFT, its inverse
//! (with its phase from `Atan`) and the output's scaling. Casts join the two. Input and output stay
//! 32-bit, so the model is used as before.
//!
//! The rewrite works on the protobuf wire format directly: fields it doesn't change are copied as
//! they are. A graph with an operator it doesn't know the types of is refused (then the model runs
//! as it is).

use std::collections::{HashMap, HashSet};

use crate::offline::ModelError;

/// Nodes under these module names are the network: 16-bit.
const NETWORK: [&str; 8] = [
    "/encoder.",
    "/decoder.",
    "/tencoder.",
    "/tdecoder.",
    "/crosstransformer",
    "/channel_upsampler",
    "/channel_downsampler",
    "/freq_emb",
];

/// Operators kept 32-bit wherever they are: statistics, the phase, and padding (which CoreML
/// leaves to the CPU, where it may have no 16-bit kernel).
const KEEP_32: [&str; 10] = [
    "ReduceMean",
    "Pow",
    "Sqrt",
    "Atan",
    "Sin",
    "Cos",
    "Where",
    "Less",
    "Greater",
    "Pad",
];

/// The operators the rewrite knows: every input and output a float tensor, but for their
/// integer arguments (shapes, axes, indices: initializers) and the booleans out of comparisons.
const KNOWN: [&str; 37] = [
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Neg",
    "Abs",
    "Exp",
    "Log",
    "Relu",
    "Tanh",
    "Erf",
    "Sigmoid",
    "Softmax",
    "Conv",
    "ConvTranspose",
    "Gemm",
    "MatMul",
    "InstanceNormalization",
    "LayerNormalization",
    "Reshape",
    "Transpose",
    "Split",
    "Slice",
    "Squeeze",
    "Unsqueeze",
    "Concat",
    "Gather",
    "Pad",
    "ReduceMean",
    "Pow",
    "Sqrt",
    "Atan",
    "Sin",
    "Cos",
    "Where",
    "Less",
    "Greater",
];

/// Operators whose output is boolean.
const BOOLEAN: [&str; 2] = ["Less", "Greater"];

const FLOAT: u64 = 1;
const FLOAT16: u64 = 10;

/// How much of the model became 16-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Converted {
    pub nodes: usize,
    pub half_nodes: usize,
    pub casts: usize,
}

/// `model` (an ONNX file's bytes) with its network in 16 bits (see the module docs).
pub fn half_precision(model: &[u8]) -> Result<(Vec<u8>, Converted), ModelError> {
    let fields = parse(model)?;
    let mut out = Vec::with_capacity(model.len() / 2 + 1024);
    let mut stats = None;
    for f in &fields {
        match (f.num, &f.val) {
            (7, Val::Bytes(g)) => {
                let (graph, s) = convert_graph(g)?;
                put_bytes(&mut out, 7, &graph);
                stats = Some(s);
            }
            _ => f.write(&mut out),
        }
    }
    let stats = stats.ok_or_else(|| err("no graph"))?;
    if stats.half_nodes == 0 {
        return Err(err("no network nodes to make 16-bit"));
    }
    Ok((out, stats))
}

fn err(why: &str) -> ModelError {
    ModelError::new(format!("16-bit model: {why}"))
}

struct Node<'a> {
    inputs: Vec<String>,
    outputs: Vec<String>,
    op: String,
    /// Every other field, as it was.
    rest: Vec<Field<'a>>,
}

struct Tensor<'a> {
    name: String,
    data_type: u64,
    fields: Vec<Field<'a>>,
}

fn convert_graph(g: &[u8]) -> Result<(Vec<u8>, Converted), ModelError> {
    let fields = parse(g)?;
    let mut nodes = Vec::new();
    let mut inits = Vec::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for f in &fields {
        match (f.num, &f.val) {
            (1, Val::Bytes(b)) => nodes.push(parse_node(b)?),
            (5, Val::Bytes(b)) => inits.push(parse_tensor(b)?),
            (11, Val::Bytes(b)) => inputs.push(value_info(b)?),
            (12, Val::Bytes(b)) => outputs.push(value_info(b)?),
            (15, _) => return Err(err("sparse initializers")),
            _ => {}
        }
    }
    let init_type: HashMap<&str, u64> = inits
        .iter()
        .map(|t| (t.name.as_str(), t.data_type))
        .collect();
    let graph_inputs: HashSet<&str> = inputs
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !init_type.contains_key(n))
        .collect();
    if inputs
        .iter()
        .any(|(n, t)| graph_inputs.contains(n.as_str()) && *t != FLOAT)
    {
        return Err(err("an input that isn't 32-bit float"));
    }

    // Which nodes are 16-bit, in order (the graph is sorted: every input is made before use).
    let mut producer: HashMap<&str, usize> = HashMap::new();
    let mut half = vec![false; nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        if !KNOWN.contains(&n.op.as_str()) {
            return Err(err(&format!("unknown operator {}", n.op)));
        }
        for t in n.inputs.iter().filter(|t| !t.is_empty()) {
            if !init_type.contains_key(t.as_str())
                && !graph_inputs.contains(t.as_str())
                && !producer.contains_key(t.as_str())
            {
                return Err(err(&format!("{t} used before it is made")));
            }
        }
        let name = node_name(n);
        let keep = KEEP_32.contains(&n.op.as_str());
        let made: Vec<usize> = n
            .inputs
            .iter()
            .filter_map(|t| producer.get(t.as_str()).copied())
            .collect();
        half[i] = !keep
            && if NETWORK.iter().any(|m| name.starts_with(m)) {
                true
            } else {
                // Glue between the network's parts: what only network nodes feed.
                !name.trim_start_matches('/').contains('/')
                    && !made.is_empty()
                    && made.iter().all(|&p| half[p])
                    && !n.inputs.iter().any(|t| graph_inputs.contains(t.as_str()))
            };
        for o in &n.outputs {
            producer.insert(o, i);
        }
    }

    // Each float tensor's names: as made (16 or 32 bits) and in the other precision when a
    // consumer needs it (a cast). Graph outputs keep their names in 32 bits.
    let graph_outputs: HashSet<&str> = outputs.iter().map(|(n, _)| n.as_str()).collect();
    let is_float = |t: &str| match producer.get(t) {
        Some(&p) => !BOOLEAN.contains(&nodes[p].op.as_str()),
        None => graph_inputs.contains(t) || init_type.get(t) == Some(&FLOAT),
    };
    let made_half = |t: &str| producer.get(t).is_some_and(|&p| half[p]);
    let name_in = |t: &str, want_half: bool| -> String {
        let renamed = graph_outputs.contains(t) && made_half(t);
        match (want_half, made_half(t)) {
            (true, true) if renamed => format!("{t}__f16"),
            (false, false) | (true, true) => t.to_string(),
            (true, false) => format!("{t}__f16"),
            (false, true) if renamed => t.to_string(),
            (false, true) => format!("{t}__f32"),
        }
    };
    // Casts wanted: (tensor, to 16 bits).
    let mut casts: Vec<(String, bool)> = Vec::new();
    let mut want_init16: HashSet<String> = HashSet::new();
    let mut want_init32: HashSet<String> = HashSet::new();
    let mut new_nodes: Vec<(Vec<String>, Vec<String>)> = Vec::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        let mut ins = Vec::with_capacity(n.inputs.len());
        for t in &n.inputs {
            if t.is_empty() || !is_float(t) {
                ins.push(t.clone());
                if init_type.contains_key(t.as_str()) {
                    want_init32.insert(t.clone());
                }
                continue;
            }
            if let Some(&dt) = init_type.get(t.as_str()) {
                debug_assert_eq!(dt, FLOAT);
                if half[i] {
                    want_init16.insert(t.clone());
                    ins.push(format!("{t}__f16"));
                } else {
                    want_init32.insert(t.clone());
                    ins.push(t.clone());
                }
                continue;
            }
            if half[i] != made_half(t) {
                let c = (t.clone(), half[i]);
                if !casts.contains(&c) {
                    casts.push(c);
                }
            }
            ins.push(name_in(t, half[i]));
        }
        let outs = n
            .outputs
            .iter()
            .map(|o| {
                if half[i] && !o.is_empty() {
                    name_in(o, true)
                } else {
                    o.clone()
                }
            })
            .collect();
        new_nodes.push((ins, outs));
    }
    for o in &graph_outputs {
        if made_half(o) {
            casts.push((o.to_string(), false));
        }
    }

    // The graph: fields in order, nodes with their casts right after the tensor is made.
    let mut out = Vec::with_capacity(g.len() / 2 + 1024);
    let mut cast_count = 0;
    let mut emit_casts = |out: &mut Vec<u8>, t: &str| {
        for (c, to_half) in casts.iter().filter(|(c, _)| c == t) {
            let (from, to) = (name_in(c, !to_half), name_in(c, *to_half));
            put_bytes(out, 1, &cast_node(&from, &to, *to_half));
            cast_count += 1;
        }
    };
    let mut nodes_written = false;
    for f in &fields {
        match f.num {
            1 => {
                if nodes_written {
                    continue;
                }
                nodes_written = true;
                for t in &graph_inputs {
                    emit_casts(&mut out, t);
                }
                for (n, (ins, outs)) in nodes.iter().zip(&new_nodes) {
                    put_bytes(&mut out, 1, &encode_node(n, ins, outs));
                    for o in &n.outputs {
                        emit_casts(&mut out, o);
                    }
                }
            }
            5 => {}  // initializers, below
            13 => {} // value_info: ONNX Runtime infers the types again
            _ => f.write(&mut out),
        }
    }
    for t in &inits {
        if t.data_type != FLOAT || want_init32.contains(&t.name) || !want_init16.contains(&t.name) {
            put_bytes(&mut out, 5, &encode_tensor(t, None)?);
        }
        if want_init16.contains(&t.name) {
            put_bytes(
                &mut out,
                5,
                &encode_tensor(t, Some(&format!("{}__f16", t.name)))?,
            );
        }
    }
    let stats = Converted {
        nodes: nodes.len(),
        half_nodes: half.iter().filter(|h| **h).count(),
        casts: cast_count,
    };
    Ok((out, stats))
}

fn node_name(n: &Node) -> String {
    n.rest
        .iter()
        .find_map(|f| match (f.num, &f.val) {
            (3, Val::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .unwrap_or_default()
}

fn parse_node(b: &[u8]) -> Result<Node<'_>, ModelError> {
    let mut n = Node {
        inputs: Vec::new(),
        outputs: Vec::new(),
        op: String::new(),
        rest: Vec::new(),
    };
    for f in parse(b)? {
        match (f.num, &f.val) {
            (1, Val::Bytes(s)) => n.inputs.push(text(s)?),
            (2, Val::Bytes(s)) => n.outputs.push(text(s)?),
            (4, Val::Bytes(s)) => {
                n.op = text(s)?;
                n.rest.push(f);
            }
            (7, Val::Bytes(s)) if !s.is_empty() => return Err(err("an operator outside ai.onnx")),
            _ => n.rest.push(f),
        }
    }
    Ok(n)
}

fn encode_node(n: &Node, inputs: &[String], outputs: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in inputs {
        put_bytes(&mut out, 1, i.as_bytes());
    }
    for o in outputs {
        put_bytes(&mut out, 2, o.as_bytes());
    }
    for f in &n.rest {
        f.write(&mut out);
    }
    out
}

fn cast_node(from: &str, to: &str, to_half: bool) -> Vec<u8> {
    let mut attr = Vec::new();
    put_bytes(&mut attr, 1, b"to");
    put_varint_field(&mut attr, 3, if to_half { FLOAT16 } else { FLOAT });
    put_varint_field(&mut attr, 20, 2); // AttributeProto.INT
    let mut out = Vec::new();
    put_bytes(&mut out, 1, from.as_bytes());
    put_bytes(&mut out, 2, to.as_bytes());
    put_bytes(&mut out, 3, format!("{to}_cast").as_bytes());
    put_bytes(&mut out, 4, b"Cast");
    put_bytes(&mut out, 5, &attr);
    out
}

fn parse_tensor(b: &[u8]) -> Result<Tensor<'_>, ModelError> {
    let fields = parse(b)?;
    let mut name = String::new();
    let mut data_type = 0;
    for f in &fields {
        match (f.num, &f.val) {
            (8, Val::Bytes(s)) => name = text(s)?,
            (2, Val::Varint(v)) => data_type = *v,
            (14, Val::Varint(1)) => return Err(err("external tensor data")),
            _ => {}
        }
    }
    Ok(Tensor {
        name,
        data_type,
        fields,
    })
}

/// The tensor as it was (`half: None`), or in 16 bits under the name `half`.
fn encode_tensor(t: &Tensor, half: Option<&str>) -> Result<Vec<u8>, ModelError> {
    let mut out = Vec::new();
    let Some(name) = half else {
        for f in &t.fields {
            f.write(&mut out);
        }
        return Ok(out);
    };
    let mut values: Vec<f32> = Vec::new();
    for f in &t.fields {
        match (f.num, &f.val) {
            (9, Val::Bytes(raw)) => {
                values.extend(
                    raw.chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
                );
            }
            (4, Val::Bytes(packed)) => {
                values.extend(
                    packed
                        .chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
                );
            }
            (4, Val::Fixed32(v)) => values.push(f32::from_bits(*v)),
            _ => {}
        }
    }
    for f in &t.fields {
        match f.num {
            2 | 4 | 8 | 9 => {}
            _ => f.write(&mut out),
        }
    }
    put_varint_field(&mut out, 2, FLOAT16);
    put_bytes(&mut out, 8, name.as_bytes());
    let raw: Vec<u8> = values
        .iter()
        .flat_map(|v| f16_bits(*v).to_le_bytes())
        .collect();
    put_bytes(&mut out, 9, &raw);
    Ok(out)
}

/// A ValueInfoProto's name and element type (0 when not a tensor).
fn value_info(b: &[u8]) -> Result<(String, u64), ModelError> {
    let mut name = String::new();
    let mut elem = 0;
    for f in parse(b)? {
        match (f.num, &f.val) {
            (1, Val::Bytes(s)) => name = text(s)?,
            (2, Val::Bytes(ty)) => {
                for tf in parse(ty)? {
                    if let (1, Val::Bytes(tensor)) = (tf.num, &tf.val) {
                        for ef in parse(tensor)? {
                            if let (1, Val::Varint(e)) = (ef.num, &ef.val) {
                                elem = *e;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok((name, elem))
}

fn text(b: &[u8]) -> Result<String, ModelError> {
    String::from_utf8(b.to_vec()).map_err(|_| err("a name that isn't UTF-8"))
}

/// IEEE half precision, rounded to nearest even (overflow to infinity, NaN kept).
pub fn f16_bits(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let man = x & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        // Subnormal: the implicit bit in, shifted down, rounded to nearest even.
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1 << (shift - 1);
        let rest = m & ((1 << shift) - 1);
        let mut r = m >> shift;
        if rest > half || (rest == half && r & 1 == 1) {
            r += 1;
        }
        return sign | r as u16;
    }
    let mut r = ((e as u32) << 10) | (man >> 13);
    let rest = man & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && r & 1 == 1) {
        r += 1; // may carry into the exponent, up to infinity: right
    }
    sign | r as u16
}

// ------------------------------------------------------------------ protobuf wire format

#[derive(Clone)]
enum Val<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

#[derive(Clone)]
struct Field<'a> {
    num: u32,
    val: Val<'a>,
}

impl Field<'_> {
    fn write(&self, out: &mut Vec<u8>) {
        match self.val {
            Val::Varint(v) => put_varint_field(out, self.num, v),
            Val::Fixed64(v) => {
                put_varint(out, (self.num as u64) << 3 | 1);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Val::Bytes(b) => put_bytes(out, self.num, b),
            Val::Fixed32(v) => {
                put_varint(out, (self.num as u64) << 3 | 5);
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
}

fn parse(mut b: &[u8]) -> Result<Vec<Field<'_>>, ModelError> {
    let bad = || err("a damaged protobuf");
    let mut out = Vec::new();
    while !b.is_empty() {
        let tag = varint(&mut b).ok_or_else(bad)?;
        let num = (tag >> 3) as u32;
        let val = match tag & 7 {
            0 => Val::Varint(varint(&mut b).ok_or_else(bad)?),
            1 => {
                let (v, r) = b.split_at_checked(8).ok_or_else(bad)?;
                b = r;
                Val::Fixed64(u64::from_le_bytes(v.try_into().unwrap()))
            }
            2 => {
                let len = varint(&mut b).ok_or_else(bad)? as usize;
                let (v, r) = b.split_at_checked(len).ok_or_else(bad)?;
                b = r;
                Val::Bytes(v)
            }
            5 => {
                let (v, r) = b.split_at_checked(4).ok_or_else(bad)?;
                b = r;
                Val::Fixed32(u32::from_le_bytes(v.try_into().unwrap()))
            }
            _ => return Err(bad()),
        };
        out.push(Field { num, val });
    }
    Ok(out)
}

fn varint(b: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let (&byte, rest) = b.split_first()?;
        *b = rest;
        v |= ((byte & 0x7f) as u64) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_varint_field(out: &mut Vec<u8>, num: u32, v: u64) {
    put_varint(out, (num as u64) << 3);
    put_varint(out, v);
}

fn put_bytes(out: &mut Vec<u8>, num: u32, b: &[u8]) {
    put_varint(out, (num as u64) << 3 | 2);
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_round_to_nearest_even() {
        assert_eq!(f16_bits(0.0), 0);
        assert_eq!(f16_bits(-0.0), 0x8000);
        assert_eq!(f16_bits(1.0), 0x3c00);
        assert_eq!(f16_bits(-2.0), 0xc000);
        assert_eq!(f16_bits(65504.0), 0x7bff);
        assert_eq!(f16_bits(1e6), 0x7c00);
        assert_eq!(f16_bits(f32::NAN) & 0x7e00, 0x7e00);
        assert_eq!(f16_bits(5.960_464_5e-8), 1, "the smallest subnormal");
        assert_eq!(f16_bits(1.0 + 1.0 / 2048.0), 0x3c00, "a tie, to even");
        assert_eq!(f16_bits(1.0 + 3.0 / 2048.0), 0x3c02, "a tie, to even (up)");
        assert_eq!(f16_bits(0.1), 0x2e66);
    }

    #[test]
    fn the_wire_format_round_trips() {
        let mut b = Vec::new();
        put_varint_field(&mut b, 2, 300);
        put_bytes(&mut b, 8, b"name");
        let fields = parse(&b).unwrap();
        let mut again = Vec::new();
        fields.iter().for_each(|f| f.write(&mut again));
        assert_eq!(again, b);
    }
}
