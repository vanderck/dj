"""Builds a tiny, hand-written (untrained) ONNX model that follows the Stems model contract v1.

It is NOT a real separator. It exists to test the ONNX path end to end:
per-frequency stem priors, plus a transient detector that compares the target frame with the
whole context window (so context and lookahead are actually exercised).

    python models/tools/make_test_model.py crates/stems-core/tests/data/test_model.onnx
"""
import sys

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

SAMPLE_RATE = 44100
FFT_SIZE = 4096
CONTEXT = 8
LOOKAHEAD = 2
F = FFT_SIZE // 2 + 1
STEMS = ["drums", "bass", "vocals", "other"]


def ramp(x, a, b):
    return np.clip((x - a) / (b - a), 0.0, 1.0)


def priors() -> np.ndarray:
    f = np.arange(F) * SAMPLE_RATE / FFT_SIZE
    low = 1.0 - ramp(f, 120.0, 300.0)
    mid = ramp(f, 200.0, 300.0) * (1.0 - ramp(f, 4000.0, 6000.0))
    logits = np.stack(
        [
            0.5 * ramp(f, 4000.0, 8000.0),  # drums: a bit of top end
            8.0 * low - 4.0,  # bass
            4.5 * mid - 3.0,  # vocals
            np.full_like(f, 0.5),  # other
        ]
    )
    return logits[None].astype(np.float32)  # [1, 4, F]


def build() -> onnx.ModelProto:
    target = CONTEXT - 1 - LOOKAHEAD
    inits = [
        numpy_helper.from_array(np.array(target, dtype=np.int64), "target_index"),
        numpy_helper.from_array(np.array(1e-9, dtype=np.float32), "eps"),
        numpy_helper.from_array(priors(), "prior"),
        numpy_helper.from_array(np.array([2.0, 0.0, -1.0, 0.0], np.float32).reshape(1, 4, 1), "transient_gain"),
    ]
    nodes = [
        helper.make_node("Gather", ["spec", "target_index"], ["target"], axis=1),  # [1,4,F]
        helper.make_node("ReduceSumSquare", ["target"], ["p_t"], axes=[1], keepdims=1),  # [1,1,F]
        helper.make_node("ReduceSumSquare", ["spec"], ["p_all"], axes=[2], keepdims=0),  # [1,T,F]
        helper.make_node("ReduceMean", ["p_all"], ["p_ctx3"], axes=[1], keepdims=1),  # [1,1,F]
        helper.make_node("Add", ["p_t", "eps"], ["p_t_e"]),
        helper.make_node("Add", ["p_ctx3", "eps"], ["p_ctx_e"]),
        helper.make_node("Log", ["p_t_e"], ["log_t"]),
        helper.make_node("Log", ["p_ctx_e"], ["log_ctx"]),
        helper.make_node("Sub", ["log_t", "log_ctx"], ["transient_raw"]),
        helper.make_node("Relu", ["transient_raw"], ["transient"]),
        helper.make_node("Mul", ["transient_gain", "transient"], ["boost"]),  # [1,4,F]
        helper.make_node("Add", ["prior", "boost"], ["logits"]),
        helper.make_node("Softmax", ["logits"], ["masks"], axis=1),
    ]
    graph = helper.make_graph(
        nodes,
        "stems_test_model",
        [helper.make_tensor_value_info("spec", TensorProto.FLOAT, [1, CONTEXT, 4, F])],
        [helper.make_tensor_value_info("masks", TensorProto.FLOAT, [1, 4, F])],
        initializer=inits,
    )
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)], producer_name="stems")
    model.ir_version = 8
    helper.set_model_props(
        model,
        {
            "stems.contract": "1",
            "stems.name": "test model (priors + transients, untrained)",
            "stems.sample_rate": str(SAMPLE_RATE),
            "stems.fft_size": str(FFT_SIZE),
            "stems.context_frames": str(CONTEXT),
            "stems.lookahead_frames": str(LOOKAHEAD),
        },
    )
    onnx.checker.check_model(model, full_check=True)
    return model


def self_test(path: str) -> None:
    import onnxruntime as ort

    sess = ort.InferenceSession(path, providers=["CPUExecutionProvider"])
    x = np.random.default_rng(0).standard_normal((1, CONTEXT, 4, F)).astype(np.float32)
    (m,) = sess.run(None, {"spec": x})
    assert m.shape == (1, 4, F), m.shape
    assert np.allclose(m.sum(axis=1), 1.0, atol=1e-5)
    f = np.arange(F) * SAMPLE_RATE / FFT_SIZE
    assert m[0, 1, f < 80].min() > 0.9, "bass should own the lows"
    print(f"ok: {path} -> masks {m.shape}, bass@60Hz={m[0, 1, int(60 * FFT_SIZE / SAMPLE_RATE)]:.3f}")


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "test_model.onnx"
    onnx.save(build(), out)
    self_test(out)
