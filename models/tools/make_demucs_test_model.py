"""Tiny stand-in for the HT-Demucs ONNX model, for stems-core's tests.

Same interface as Mixxx's htdemucs.onnx ([1, 2, N] -> [1, 4, 2, N], sources drums, bass, other,
vocals) with N = 1000, and each source a fixed fraction of the input at the same sample, so any
misplaced chunk, padding or trim shows up as an error.

    python models/tools/make_demucs_test_model.py crates/stems-core/tests/data/demucs_test.onnx

also writes demucs_test.<stem>.onnx next to it: a "bag" like HT-Demucs FT, one file per stem.
"""
import sys

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

SEGMENT = 1000
GAINS = [0.1, 0.2, 0.3, 0.4]  # drums, bass, other, vocals


def build(stem=None):
    gains = numpy_helper.from_array(np.array(GAINS, np.float32).reshape(1, 4, 1, 1), "gains")
    axes = numpy_helper.from_array(np.array([1], np.int64), "axes")
    graph = helper.make_graph(
        [helper.make_node("Unsqueeze", ["input", "axes"], ["x4"]),
         helper.make_node("Mul", ["x4", "gains"], ["output"])],
        "demucs_test",
        [helper.make_tensor_value_info("input", TensorProto.FLOAT, [1, 2, SEGMENT])],
        [helper.make_tensor_value_info("output", TensorProto.FLOAT, [1, 4, 2, SEGMENT])],
        initializer=[gains, axes],
    )
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)])
    model.ir_version = 8
    if stem:
        helper.set_model_props(model, {"demucs.stem": stem})
    onnx.checker.check_model(model)
    return model


if __name__ == "__main__":
    out = sys.argv[1]
    onnx.save(build(), out)
    for stem in ("drums", "bass", "other", "vocals"):
        onnx.save(build(stem), out.replace(".onnx", f".{stem}.onnx"))
