"""Stand-ins for the packages allin1 imports on load but this conversion doesn't use."""
import sys, types, importlib.abc, importlib.machinery

ROOTS = {'demucs', 'hydra', 'librosa', 'lightning', 'madmom', 'matplotlib', 'mir_eval', 'pandas', 'scipy', 'sklearn', 'timm', 'tqdm', 'wandb'}

class _Any(types.ModuleType):
    def __getattr__(self, name):
        if name.startswith('__'):
            raise AttributeError(name)
        return _Any(f'{self.__name__}.{name}')
    def __call__(self, *a, **k):
        return _Any('call')
    def __mro_entries__(self, bases):
        return (object,)

class _Finder(importlib.abc.MetaPathFinder, importlib.abc.Loader):
    def find_spec(self, name, path, target=None):
        if name.split('.')[0] in ROOTS:
            return importlib.machinery.ModuleSpec(name, self, is_package=True)
        return None
    def create_module(self, spec):
        m = _Any(spec.name)
        m.__path__ = []
        return m
    def exec_module(self, module):
        pass

sys.meta_path.insert(0, _Finder())
