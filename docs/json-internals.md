# SQLite JSON Internals (`json.c`)

Reference: `~/Downloads/sqlite-src-3530200/src/json.c` (5734 lines).

## Two representations

SQLite stores JSON in **two** forms internally:

1. **Text JSON** — the canonical RFC 8259 text the user passes in (`'{"a":1}'`).
2. **JSONB** — a compact binary form. A JSONB blob is a sequence of nodes, each a 1-byte
   header (4-bit type / 4-bit payload-size-class) followed by an optional payload-size varint
   and the payload bytes. The type codes are `JSONB_NULL=0`, `JSONB_TRUE=1`, `JSONB_FALSE=2`,
   `JSONB_INT=3`, `JSONB_FLOAT=4`, `JSONB_TEXT=5` (TEXTJ/TEXT5/TEXTRAW variants in the high
   nibble for JSON5 vs canonical escaping), `JSONB_ARRAY=6`, `JSONB_OBJECT=7`.

Every `json_*` function parses the input text into a `JsonParse` holding the JSONB blob
(`aBlob`/`nBlob`), then operates on the blob, then renders the blob back to text via
`jsonTranslateBlobToText`. The text form is kept around (`zJson`) for path lookups that need
the original label text.

## `JsonParse` struct (line 353)

```c
struct JsonParse {
  u8 *aBlob;         /* JSONB bytes */
  u32 nBlob;         /* used bytes */
  u32 nBlobAlloc;    /* allocated bytes (0 if external) */
  char *zJson;       /* original text */
  int nJson;         /* text length */
  u32 iErr;          /* error offset */
  u16 iDepth;        /* current nesting depth */
  u8 nErr;           /* error count */
  u8 hasNonstd;      /* JSON5 features used */
  u8 bReadOnly;      /* no modifications */
  u8 eEdit;          /* JEDIT_DEL/REPL/INS/SET/AINS */
  int delta;         /* size change from pending edit */
};
```

## Parser (`jsonTranslateTextToBlob`, line 1581)

Recursive descent. Return-value convention:
- `>0`: the byte offset *past* the parsed value (success).
- `-1`: malformed JSON; `pParse->iErr` is the bad byte.
- `-2`: end-of-object (`}`) seen; `pParse->iErr` is its offset.
- `-3`: end-of-array (`]`).
- `-4`: comma (continue array/object).
- `-5`: colon (key/value separator).

JSON5 extensions are gated on `hasNonstd`: single-quoted strings, unquoted keys, trailing
commas, `Infinity`/`NaN`, hex literals, `+`-prefixed numbers, line/paragraph separator
escapes (`\u2028`/`\u2029`), and control characters inside strings.

## Depth limit

`JSON_MAX_DEPTH = 1000` (line 391). Exceeding it is a malformed-JSON error.

## Rendering (`jsonTranslateBlobToText`, line 2193)

Walks the JSONB blob, emitting canonical JSON text. Strings are escaped with the short
escapes `\"`/`\\`/`\b`/`\f`/`\n`/`\r`/`\t` and `\u00XX` for other control chars; non-ASCII
passes through as UTF-8 (no `\u` escaping above 0x7F). Numbers use SQLite's faithful
`%!0.17g`/`%!.17g` REAL formatter (our `util::fp::fp_to_text`).

## Subtype

Text values produced by `json()`/`json_extract()`/etc. carry `JSON_SUBTYPE = 74` (ASCII 'J')
via `sqlite3_result_subtype()`/`sqlite3_value_subtype()`. This marks a TEXT value as "already
JSON" so it's not re-quoted when passed back into another `json_*` function. The Rust engine
models this via `Mem::subtype` (M24.20).

## Path notation

`json_extract(X, P1, P2, ...)` walks each path `Pi` against the parsed tree. Path syntax:
- `$` — the root.
- `$.key` / `$["key"]` — object lookup.
- `$[i]` / `$[i:j]` — array index / slice (negative `i` counts from the end).
- `$.a.b[2].c` — chained.

Path lookup is in `jsonLookupStep` (line ~3000). The `eEdit`/`delta` machinery is for
`json_insert`/`json_replace`/`json_set`/`json_remove` — they patch the JSONB blob in place.

## Rustqlite implementation note (M24.1)

For M24.1 we implement a strict RFC 8259 recursive-descent parser producing an owned
`JsonNode` tree (`func::json::JsonNode`), without the JSONB binary form. This is the
foundation every M24.2–M24.19 function builds on. The JSONB form is a large optimization
surface (cache-friendly, in-place edits) that is not needed for correctness and can land
later. The parser runs on a 64 MiB-stack worker thread when the input's nesting depth exceeds
200, so the `JSON_MAX_DEPTH=1000` recursion limit cannot overflow the default 2 MiB thread
stack in debug builds (where Rust frames are large).

## Known divergences from the C oracle (M24.1/M24.2 tree-parser approach)

These all stem from upstream's JSONB form storing the **original text** of strings and
numbers verbatim and re-rendering it on output, while our tree parser **decodes** during
parsing and re-renders from the decoded value. They are not bugs — they are the cost of the
tree approach, and will resolve when the JSONB form lands.

1. **`\u` escapes preserved verbatim.** `json('"\u0041"')` returns `"\u0041"` upstream but
   `"A"` with our parser. The JSONB blob stores the raw string bytes; we decode `\u0041` to
   `A` during parsing and re-render `A`.
2. **Number text preserved verbatim.** `json('1e10')` returns `1e10` upstream but
   `10000000000.0` with our parser; `json('9223372036854775808')` returns the integer text
   upstream but `9.2233720368547758e+18` with our parser (i64 overflow promotes to f64 and
   re-renders via `fp_to_text`). Numbers that already match `fp_to_text`'s output (e.g.
   `1.5`, `-1.5`) round-trip identically.
3. **JSON5 extensions rejected.** Upstream accepts JSON5 by default (single-quoted strings,
   unquoted keys, trailing commas, `Infinity`/`NaN`, hex literals, `+`-prefixed numbers,
   leading-dot reals, comments). Our parser is strict RFC 8259 and rejects them. The
   `hasNonstd` flag in upstream tracks JSON5 use; we don't accept it at all.

The differential test `json_function` in `tests/diff.rs` skips the divergent cases with a
comment pointing here.