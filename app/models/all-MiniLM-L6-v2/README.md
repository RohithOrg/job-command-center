# all-MiniLM-L6-v2 (bundled)

Built-in sentence-embedding model used by `src/embed.rs`. These files are compiled
into the app binary with `include_bytes!`, so nothing is read or downloaded at runtime.

- Source: https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2
- Authors: sentence-transformers (Nils Reimers et al.), based on
  `nreimers/MiniLM-L6-H384-uncased` (Microsoft MiniLM)
- License: Apache License 2.0 (see `LICENSE`)
- Architecture: 6-layer BERT, hidden size 384, max 256 tokens; mean pooling + L2 normalisation

## Files

| File | Notes |
|------|-------|
| `tokenizer.json` | Unmodified from upstream |
| `config.json` | Unmodified from upstream |
| `model.f16.safetensors` | Modified: upstream `model.safetensors` (f32, sha256 `53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db`) converted to float16; the unused `pooler.*` and `embeddings.position_ids` tensors were dropped. Upcast to f32 at load time. |

SHA-256 of the bundled files:

```
be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037  tokenizer.json
953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41  config.json
ef4f66adbac55a55f9d5a2d865d84a17f42e2f202b49d0235c77d3cdd5b19ce9  model.f16.safetensors
```

Verified against the Python `sentence-transformers` reference: cosine similarity
>= 0.99999 on all test sentences, max absolute per-dimension difference about 2e-4.
