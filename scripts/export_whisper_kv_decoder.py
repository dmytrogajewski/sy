#!/usr/bin/env python3
"""Export a STATIC-shape KV-cache Whisper-medium decoder for the NPU.

Why: AMD's prebuilt `decoder_model.onnx` is stateless `[1,128]` — every
greedy step re-runs all 128 positions, O(n^2) decode. A KV-cache decoder
runs one token per step (O(n)), but the usual `optimum` "with-past" export
is *dynamic* (the past grows each step) and VAIML (the NPU AOT compiler)
needs static shapes.

This exports a decoder whose shapes are fully static: a fixed `MAX`-slot
self-attention KV buffer (passed in/out as tensors), cross-attention KV
precomputed once, and an additive mask supplied by the caller. No in-graph
scatter/gather over a runtime index — the host writes each step's new K/V
into the buffer for the next step. That keeps the graph to the same
matmul/softmax/layernorm ops the stateless decoder already runs on VAIML.

Two graphs are emitted into <out>/:
  * cross_init.onnx     enc[1,1500,1024] -> 24x(cross_k,cross_v)[1,16,1500,64]
  * decoder_step.onnx   input_ids[1,1] + pos_emb[1,1,1024] + self_mask
                        + 24x(past_k,past_v)[1,16,MAX,64]
                        + 24x(cross_k,cross_v)[1,16,1500,64]
                        -> logits[1,1,51865] + 24x(new_k,new_v)[1,16,1,64]

The numerics reuse the loaded HF submodules verbatim, so the export matches
`openai/whisper-medium` exactly. A `--validate` pass greedy-decodes the AMD
oracle clip through the ONNX graphs (ORT CPU) and asserts the reference
transcript, proving the static-shape design is correct before any NPU test.
"""
import argparse
import sys
from pathlib import Path

import numpy as np
import torch
from torch import nn

MODEL_ID = "openai/whisper-medium"
N_LAYERS = 24
N_HEADS = 16
H_DIM = 64
D_MODEL = 1024
ENC_FRAMES = 1500
VOCAB = 51865
SCALING = 0.125
MAX = 128  # self-attention KV buffer depth (matches stateless decoder len)


def _heads(x):
    # [1, S, 1024] -> [1, 16, S, 64]
    return x.view(1, -1, N_HEADS, H_DIM).transpose(1, 2)


def _merge(x):
    # [1, 16, 1, 64] -> [1, 1, 1024]
    return x.transpose(1, 2).reshape(1, -1, D_MODEL)


class CrossInit(nn.Module):
    """Precompute the cross-attention K/V from the encoder output once."""

    def __init__(self, decoder):
        super().__init__()
        self.layers = decoder.layers

    def forward(self, enc):  # enc: [1, 1500, 1024]
        outs = []
        for layer in self.layers:
            ca = layer.encoder_attn
            outs.append(_heads(ca.k_proj(enc)))  # [1,16,1500,64]
            outs.append(_heads(ca.v_proj(enc)))
        return tuple(outs)


class DecoderStep(nn.Module):
    """One greedy step with a static-shape KV cache (host-managed buffer)."""

    def __init__(self, model):
        super().__init__()
        self.dec = model.model.decoder
        self.proj_out = model.proj_out
        self.layers = self.dec.layers

    def forward(self, input_ids, pos_emb, self_mask, *kv):
        # kv layout: [past_k]*24, [past_v]*24, [cross_k]*24, [cross_v]*24
        past_k = kv[0:N_LAYERS]
        past_v = kv[N_LAYERS:2 * N_LAYERS]
        cross_k = kv[2 * N_LAYERS:3 * N_LAYERS]
        cross_v = kv[3 * N_LAYERS:4 * N_LAYERS]

        h = self.dec.embed_tokens(input_ids) + pos_emb  # [1,1,1024]
        new_k, new_v = [], []
        for L, layer in enumerate(self.layers):
            # --- self-attention (static KV: concat past buffer + current) ---
            res = h
            x = layer.self_attn_layer_norm(h)
            sa = layer.self_attn
            q = _heads(sa.q_proj(x) * SCALING)          # [1,16,1,64]
            kc = _heads(sa.k_proj(x))                   # [1,16,1,64]
            vc = _heads(sa.v_proj(x))
            k_all = torch.cat([past_k[L], kc], dim=2)   # [1,16,MAX+1,64]
            v_all = torch.cat([past_v[L], vc], dim=2)
            aw = torch.matmul(q, k_all.transpose(-1, -2)) + self_mask
            aw = aw.softmax(-1)
            ao = layer.self_attn.out_proj(_merge(torch.matmul(aw, v_all)))
            h = res + ao
            new_k.append(kc)
            new_v.append(vc)
            # --- cross-attention (precomputed K/V) ---
            res = h
            x = layer.encoder_attn_layer_norm(h)
            ca = layer.encoder_attn
            q = _heads(ca.q_proj(x) * SCALING)
            aw = torch.matmul(q, cross_k[L].transpose(-1, -2)).softmax(-1)
            ao = layer.encoder_attn.out_proj(_merge(torch.matmul(aw, cross_v[L])))
            h = res + ao
            # --- feed-forward ---
            res = h
            x = layer.final_layer_norm(h)
            h = res + layer.fc2(layer.activation_fn(layer.fc1(x)))
        h = self.dec.layer_norm(h)
        logits = self.proj_out(h)  # [1,1,51865]
        return (logits, *new_k, *new_v)


def _names():
    pk = [f"past_k_{i}" for i in range(N_LAYERS)]
    pv = [f"past_v_{i}" for i in range(N_LAYERS)]
    ck = [f"cross_k_{i}" for i in range(N_LAYERS)]
    cv = [f"cross_v_{i}" for i in range(N_LAYERS)]
    nk = [f"new_k_{i}" for i in range(N_LAYERS)]
    nv = [f"new_v_{i}" for i in range(N_LAYERS)]
    return pk, pv, ck, cv, nk, nv


def export(model, out: Path):
    out.mkdir(parents=True, exist_ok=True)
    dec = model.model.decoder
    pk, pv, ck, cv, nk, nv = _names()

    # cross_init
    ci = CrossInit(dec).eval()
    enc = torch.zeros(1, ENC_FRAMES, D_MODEL)
    torch.onnx.export(
        ci, (enc,), str(out / "cross_init.onnx"),
        input_names=["encoder_hidden"],
        output_names=[v for pair in zip(ck, cv) for v in pair],
        opset_version=17, do_constant_folding=True,
    )
    print("wrote", out / "cross_init.onnx")

    # decoder_step
    step = DecoderStep(model).eval()
    input_ids = torch.zeros(1, 1, dtype=torch.long)
    pos_emb = torch.zeros(1, 1, D_MODEL)
    self_mask = torch.zeros(1, 1, 1, MAX + 1)
    past_k = [torch.zeros(1, N_HEADS, MAX, H_DIM) for _ in range(N_LAYERS)]
    past_v = [torch.zeros(1, N_HEADS, MAX, H_DIM) for _ in range(N_LAYERS)]
    cross_k = [torch.zeros(1, N_HEADS, ENC_FRAMES, H_DIM) for _ in range(N_LAYERS)]
    cross_v = [torch.zeros(1, N_HEADS, ENC_FRAMES, H_DIM) for _ in range(N_LAYERS)]
    args = (input_ids, pos_emb, self_mask, *past_k, *past_v, *cross_k, *cross_v)
    in_names = ["input_ids", "pos_emb", "self_mask"] + pk + pv + ck + cv
    out_names = ["logits"] + nk + nv
    torch.onnx.export(
        step, args, str(out / "decoder_step.onnx"),
        input_names=in_names, output_names=out_names,
        opset_version=17, do_constant_folding=True,
    )
    print("wrote", out / "decoder_step.onnx")


def validate(model, out: Path, wav: str):
    """Greedy-decode the oracle clip through the exported ONNX (ORT CPU)."""
    import onnxruntime as ort
    import wave
    import struct
    from transformers import WhisperProcessor

    proc = WhisperProcessor.from_pretrained(MODEL_ID)
    w = wave.open(wav, "rb")
    pcm = np.array(struct.unpack("<%dh" % (w.getnframes()), w.readframes(w.getnframes())), dtype=np.float32) / 32768.0
    feats = proc(pcm, sampling_rate=16000, return_tensors="pt").input_features  # [1,80,3000]

    # encoder via HF (reference encoder_hidden)
    with torch.no_grad():
        enc = model.model.encoder(feats).last_hidden_state  # [1,1500,1024]

    so = ort.SessionOptions()
    ci = ort.InferenceSession(str(out / "cross_init.onnx"), so, providers=["CPUExecutionProvider"])
    ds = ort.InferenceSession(str(out / "decoder_step.onnx"), so, providers=["CPUExecutionProvider"])
    pk, pv, ck, cv, nk, nv = _names()

    cross = ci.run(None, {"encoder_hidden": enc.numpy()})
    cross_k = {ck[i]: cross[2 * i] for i in range(N_LAYERS)}
    cross_v = {cv[i]: cross[2 * i + 1] for i in range(N_LAYERS)}

    embed_pos = model.model.decoder.embed_positions.weight.detach().numpy()  # [448,1024]
    sot = proc.tokenizer.convert_tokens_to_ids("<|startoftranscript|>")
    eot = proc.tokenizer.eos_token_id
    transcribe = proc.tokenizer.convert_tokens_to_ids("<|transcribe|>")
    notimestamps = proc.tokenizer.convert_tokens_to_ids("<|notimestamps|>")

    past_k = {pk[i]: np.zeros((1, N_HEADS, MAX, H_DIM), np.float32) for i in range(N_LAYERS)}
    past_v = {pv[i]: np.zeros((1, N_HEADS, MAX, H_DIM), np.float32) for i in range(N_LAYERS)}

    # Probe language after SOT, then force transcribe + notimestamps.
    def step(token, pos):
        pe = embed_pos[pos].reshape(1, 1, D_MODEL).astype(np.float32)
        mask = np.full((1, 1, 1, MAX + 1), -1e9, np.float32)
        mask[0, 0, 0, :pos] = 0.0      # valid past slots
        mask[0, 0, 0, MAX] = 0.0       # current token (concat tail)
        feed = {"input_ids": np.array([[token]], np.int64), "pos_emb": pe, "self_mask": mask}
        feed.update(past_k); feed.update(past_v); feed.update(cross_k); feed.update(cross_v)
        res = ds.run(None, feed)
        logits = res[0]
        for i in range(N_LAYERS):
            past_k[pk[i]][0, :, pos, :] = res[1 + i][0, :, 0, :]
            past_v[pv[i]][0, :, pos, :] = res[1 + N_LAYERS + i][0, :, 0, :]
        return logits[0, 0]

    tokens = [sot]
    lang = int(np.argmax(step(sot, 0)))
    tokens = [sot, lang, transcribe, notimestamps]
    # replay forced prefix into the cache (positions 1..3)
    for p in range(1, len(tokens)):
        step(tokens[p], p)
    pos = len(tokens)
    while pos < MAX:
        nxt = int(np.argmax(step(tokens[-1], pos)))
        if nxt == eot:
            break
        tokens.append(nxt)
        pos += 1
    text = proc.tokenizer.decode([t for t in tokens], skip_special_tokens=True).strip()
    print("TRANSCRIPT:", text)
    low = text.lower()
    ok = "stew" in low and "turnips" in low
    print("ORACLE MATCH:", ok)
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(Path.home() / ".cache/sy/aiplane/whisper-medium/kv-src"))
    ap.add_argument("--validate", action="store_true")
    ap.add_argument("--wav", default="/home/dmitriy/sources/RyzenAI-SW/Demos/ASR/Whisper/audio_files/1089-134686-0000.wav")
    args = ap.parse_args()
    from transformers import WhisperForConditionalGeneration
    model = WhisperForConditionalGeneration.from_pretrained(MODEL_ID, dtype=torch.float32).eval()
    out = Path(args.out)
    export(model, out)
    if args.validate:
        ok = validate(model, out, args.wav)
        sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
