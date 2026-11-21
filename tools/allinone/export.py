"""Export All-In-One's "harmonix-all" (eight folds, averaged) to ONNX for dj:
crates/stems-core/models/allinone.onnx.

    python -m venv venv && venv/bin/pip install torch onnx onnxscript numpy omegaconf
    venv/bin/pip install --no-deps allin1==1.1.0
    ALLIN1_WEIGHTS=folder/with/harmonix-fold*.pth venv/bin/python tools/allinone/export.py out.onnx

NATTEN's kernels are replaced by `natten/functional.py` (plain gathers, same neighbourhoods), so
the graph has only standard operators and a dynamic length (at least 10240 frames)."""
import sys, os, torch, onnx
sys.path.insert(0, os.path.dirname(__file__))
import load  # noqa
from allin1.models.loaders import load_pretrained_model


class Averaged(torch.nn.Module):
    """The eight trained models, averaged as allin1's Ensemble does: logits (beat, downbeat,
    section, function) for spectrograms (1, 4, T, 81)."""
    def __init__(self):
        super().__init__()
        self.models = torch.nn.ModuleList([load_pretrained_model(f'harmonix-fold{i}', device='cpu') for i in range(8)])

    def forward(self, spec):
        outs = [m(spec) for m in self.models]
        avg = lambda k: torch.stack([getattr(o, k) for o in outs]).mean(0)
        return avg('logits_beat'), avg('logits_downbeat'), avg('logits_section'), avg('logits_function')


if __name__ == '__main__':
    out = sys.argv[1]
    m = Averaged().eval()
    T = torch.export.Dim('frames', min=10240, max=200000)
    x = torch.randn(1, 4, 12000, 81).abs()
    with torch.no_grad():
        torch.onnx.export(m, (x,), out + '.full', dynamo=True, dynamic_shapes={'spec': {2: T}},
                          input_names=['spec'], output_names=['beat', 'downbeat', 'section', 'function'], optimize=True)
    # One file, without the exporter's per-node metadata (30 MB of it).
    g = onnx.load(out + '.full')
    for n in g.graph.node:
        del n.metadata_props[:]
    del g.graph.value_info[:]
    del g.metadata_props[:]
    g.doc_string = ('All-In-One (Kim & Nam 2023), harmonix-all: 8 folds averaged. Input spec (1,4,T>=10240,81): '
                    'log-filtered spectrograms of bass, drums, other, vocals. Outputs logits: beat, downbeat, section (1,T), function (1,10,T).')
    onnx.checker.check_model(g)
    onnx.save(g, out)
    for f in (out + '.full', out + '.full.data'):
        if os.path.exists(f):
            os.remove(f)
    print('exported', os.path.getsize(out) // 1024, 'KiB')
