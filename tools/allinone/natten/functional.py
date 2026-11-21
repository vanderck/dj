"""Pure-PyTorch neighborhood attention (NATTEN's natten1d/2d qkrpb and av), so the model can run
and export to ONNX without NATTEN's custom kernels. Same neighborhoods as NATTEN: a window of
`kernel_size` positions `dilation` apart around each query, shifted (not padded) at the edges,
with relative position biases indexed the same way."""
import torch


def _starts(length: int, kernel_size: int, dilation: int):
    """Per position: the first neighbor's index, and the first relative-position-bias index."""
    n = kernel_size // 2
    win, pb = [], []
    for i in range(length):
        if dilation <= 1:
            s = max(i - n, 0) + (i + n >= length) * (length - i - n - 1)
            p = n + (i < n) * (n - i) + (i + n >= length) * (length - i - 1 - n)
        else:
            ni = i - n * dilation
            if ni < 0:
                s = i % dilation
            elif i + n * dilation >= length:
                imodd = i % dilation
                a = (length // dilation) * dilation
                b = length - a
                s = length - b + imodd - 2 * n * dilation if imodd < b else a + imodd - kernel_size * dilation
            else:
                s = ni
            if i - n * dilation < 0:
                p = kernel_size - 1 - (i // dilation)
            elif i + n * dilation >= length:
                p = (length - i - 1) // dilation
            else:
                p = n
        win.append(s)
        pb.append(p)
    return win, pb


def _index(length, kernel_size, dilation, device=None):
    """The neighbor indices (L, K) and relative-position-bias indices (L, K), with tensor
    operations only, so an ONNX export keeps the length dynamic. Same as `_starts`."""
    n = kernel_size // 2
    d = dilation
    i = torch.arange(length, device=device)
    L = i.new_full((), 0) + length
    if d <= 1:
        s = torch.clamp(i - n, min=0) + (i + n >= L).long() * (L - i - n - 1)
        p = n + (i < n).long() * (n - i) + (i + n >= L).long() * (L - i - 1 - n)
    else:
        ni = i - n * d
        imodd = torch.remainder(i, d)
        a = torch.div(L, d, rounding_mode='floor') * d
        b = L - a
        s_end = torch.where(imodd < b, L - b + imodd - 2 * n * d, a + imodd - kernel_size * d)
        end = i + n * d >= L
        s = torch.where(ni < 0, imodd, torch.where(end, s_end, ni))
        p = torch.where(ni < 0, kernel_size - 1 - torch.div(i, d, rounding_mode='floor'), torch.where(end, torch.div(L - i - 1, d, rounding_mode='floor'), i.new_full((), n)))
    k = torch.arange(kernel_size, device=device)
    return s[:, None] + k[None, :] * d, p[:, None] + k[None, :]


def _take(x, dim, idx):
    """x gathered along `dim` at idx (any shape): one Gather, no transposes, when exported."""
    out = x.index_select(dim, idx.reshape(-1))
    return out.reshape(*x.shape[:dim], *idx.shape, *x.shape[dim + 1:])


def natten1dqkrpb(query, key, rpb, kernel_size, dilation):
    # query, key: B, H, L, D ; rpb: H, 2K-1 -> B, H, L, K
    L = query.shape[2]
    nbr, rel = _index(L, kernel_size, dilation)
    k = _take(key, 2, nbr)  # B, H, L, K, D
    attn = (query.unsqueeze(3) * k).sum(-1)
    return attn + _take(rpb, 1, rel)  # H, L, K broadcast


def natten1dav(attn, value, kernel_size, dilation):
    # attn: B, H, L, K ; value: B, H, L, D -> B, H, L, D
    L = value.shape[2]
    nbr, _ = _index(L, kernel_size, dilation)
    v = _take(value, 2, nbr)  # B, H, L, K, D
    return (attn.unsqueeze(-1) * v).sum(3)


def natten2dqkrpb(query, key, rpb, kernel_size, dilation):
    # query, key: B, H, X, Y, D ; rpb: H, 2K-1, 2K-1 -> B, H, X, Y, K*K
    X, Y = query.shape[2], query.shape[3]
    nx, rx = _index(X, kernel_size, dilation)
    ny, ry = _index(Y, kernel_size, dilation)
    k = _take(_take(key, 2, nx), 4, ny)  # B, H, X, Kx, Y, Ky, D
    attn = (query[:, :, :, None, :, None, :] * k).sum(-1)  # B, H, X, Kx, Y, Ky
    attn = attn + _take(_take(rpb, 1, rx), 3, ry)  # H, X, Kx, Y, Ky
    attn = attn.permute(0, 1, 2, 4, 3, 5)  # B, H, X, Y, Kx, Ky
    return attn.reshape(*attn.shape[:4], kernel_size * kernel_size)


def natten2dav(attn, value, kernel_size, dilation):
    # attn: B, H, X, Y, K*K ; value: B, H, X, Y, D -> B, H, X, Y, D
    X, Y = value.shape[2], value.shape[3]
    nx, _ = _index(X, kernel_size, dilation)
    ny, _ = _index(Y, kernel_size, dilation)
    v = _take(_take(value, 2, nx), 4, ny)  # B, H, X, Kx, Y, Ky, D
    a = attn.reshape(*attn.shape[:4], kernel_size, kernel_size).permute(0, 1, 2, 4, 3, 5)  # B, H, X, Kx, Y, Ky
    return (a[..., None] * v).sum((3, 5))
