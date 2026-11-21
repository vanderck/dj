"""Load allin1 without its training dependencies, with the weights from a local folder
(ALLIN1_WEIGHTS: the harmonix-fold{0..7}-*.pth files from huggingface.co/taejunkim/allinone)."""
import sys, os, types
sys.path.insert(0, os.path.dirname(__file__))
import stubs  # noqa
hub = types.ModuleType('huggingface_hub')
hub.hf_hub_download = lambda repo_id, filename, cache_dir=None: os.path.join(os.environ['ALLIN1_WEIGHTS'], filename)
sys.modules['huggingface_hub'] = hub
from allin1.models.loaders import load_pretrained_model  # noqa
