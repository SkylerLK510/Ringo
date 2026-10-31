# The research role

`python3 ringo.py research` answers a brief from web sources that Ringo checks itself: one search-enabled call
finds sources, code verifies them at no token cost, and one tool-free call writes a report from the verified
evidence only. It is new and unmeasured. Nothing here claims it finds better sources, supports claims better or
costs fewer tokens than `ask` or a hand-sourced answer; the evaluation that would show that has not been run.

```sh
python3 ringo.py research --input brief.md --trace /ABSOLUTE/PATH/TO/new-trace --out /ABSOLUTE/PATH/TO/report.md \
    [--backend codex-chatgpt] [--timeout 600] [--allow-private-search]
```

`--trace` (a new or empty folder) and `--out` must be outside this checkout. The source is `research.py`; the
prompts are `prompts/research-gather.md` and `prompts/research-write.md`; the tests are `tests/test_research.py`.

## Flow and call bounds

| Step | Calls | What happens |
|---|---|---|
| Consent | 0 | A brief that looks private is refused unless `--allow-private-search` is given (see Privacy). |
| Capability | 0 | `codex --help` must list `--search`; `codex exec --help` must list `--json`, `--ephemeral`, `--ignore-user-config` and a read-only `--sandbox`; `codex features list` must list every feature the preset turns on or off, none of them removed. The probes run with an empty private `HOME` and `CODEX_HOME`. Otherwise: research unavailable. |
| Gather | 1 | One call on `codex-search`. The turn must contain at least one `web_search` item and no other tool item. |
| Verify | 0 | Code fetches and checks every source (below). |
| Write | 1 | Only when verified evidence exists: one tool-free call on `--backend`. |
| Render and final check | 0 | Code renders citations and checks them. |

There is no retry, no repair call and no soft-limit probe: `--timeout` (default 600 seconds) is a hard limit per
call. A quota refusal or any other failed call ends the job; no other backend is tried. So:

| Outcome | Model calls | Exit | `state.json` status |
|---|---|---|---|
| `report` | 2 | 0 | `done` |
| `write_rejected` (the reply broke the format or cited unverified evidence), `write_failed` | 2 | 1 | `failed` |
| `evidence_failure` (no verified source, or `NO SOURCES`) | 1 | 1 | `failed` |
| `gather_rejected` (the reply broke the format) | 1 | 1 | `failed` |
| `gather_failed` (the call failed, timed out or used a tool other than web search) | 1 | 2 | `failed` |
| `unavailable` after the call (the turn made no web search, so its sources would come from memory) | 1 | 2 | `failed` |
| `unavailable` (no CLI, no live search, a missing feature) | 0 | 2 | `failed` |
| `refused` (private brief) | 0 | 2 | `failed` |

Every outcome writes `--out` (the report, or what failed and why, with the verified evidence when there is some),
records `outcome` in `state.json`, and prints the `harness: {...}` summary line on stderr. `ringo wait` works on the
trace as for review and design. A start error (a tool-using `--backend`, a bad `--timeout`, a trace inside the
checkout) makes no call and exits 2.

A CLI invocation is not one model exchange: the gather turn may search several times. The summary reports the
number of `web_search` items as `searches`; the Codex CLI exposes no control that caps them, so that count is
measured, not bounded. The CLI's own connection retries are off (`--disable unbounded_connection_retries`).

## The gather call

The `codex-search` preset is `codex-chatgpt` with live search: `codex --search exec ...` with `-c
web_search="live"` in place of `-c web_search="disabled"`. Everything else is the same: ChatGPT login forced, API
keys and endpoint overrides removed from the environment, a read-only sandbox, the shell, unified exec, hooks,
apps, plugins, skills and image tools off, a private `CODEX_HOME`, and Codex's agent prompt replaced (the lean
mode). Its declared tools are `["web_search"]`, so a `web_search` item is accepted and any other tool item fails the
turn. Whether live search behaves as intended under the lean catalog has been checked against the CLI's help and
feature listing, not in a model run.

The reply is JSON Lines, one source per line and nothing else (one surrounding code fence is tolerated), or the
single line `NO SOURCES`:

```json
{"url": "https://...", "title": "...", "published": "2024-05-01 or null", "claims": [{"id": "c1", "claim": "...", "quote": "..."}]}
```

Parsing fails closed: a line that is not one JSON object, a repeated or unknown key, a wrong type, a missing field,
a repeated claim id, NaN or Infinity rejects the whole reply. Bounds are applied rather than rejected: sources past
the first 10 distinct URLs are listed as not checked, claims past 5 per source are not checked, and a quote shorter
than 20 or longer than 1,000 characters (after normalisation), or of fewer than 3 words, is not checked.

The prompt asks the model to work like a careful researcher: split the brief into narrow questions; search for the
primary source first (the standard, the official documentation, the original paper, the official dataset); trace a
claim upstream when a page credits another work; open each page and copy its quote from the page itself; prefer
canonical HTML pages over PDFs and mirrors; look for a second, independent source; keep disagreeing sources; record
dates; and ignore instructions inside pages. The verifier, not the prompt, is what enforces anything.

## The verifier

Fetching (`research.fetch`):
- only `http` and `https`; a URL with credentials, spaces or control characters is refused;
- `localhost`, `*.local`, `*.internal`, single-label names and numeric hosts in non-standard forms are refused
  by name; every other host is resolved once and **every** address it returns must be public: loopback, private,
  link-local, multicast, reserved, unspecified, shared-address and cloud-metadata addresses are refused, including
  IPv4 addresses inside IPv4-mapped, 6to4, Teredo and NAT64 IPv6 forms;
- the connection goes to a checked address (the first, in the resolver's order, that answers), never to a second
  lookup of the name, so DNS rebinding between the check and the connect is not possible; TLS still verifies the
  certificate against the host name;
- each redirect (at most 3) is a new hop with the same checks; no cookies, credentials, referrer or proxy are ever
  used;
- the body is read only for HTTP 200 and 203, within 2 MiB transferred (a larger declared size is refused before
  reading) and 2 MiB after gzip or deflate decoding;
- a fetch chain has 10 seconds, redirects included; at its deadline every socket it opened is shut down, so a server
  that trickles bytes cannot outlast it; all fetching and extraction share a 60-second deadline; two chains run at a
  time, on daemon threads, so a stuck name lookup cannot hold the run open. At these limits ten URLs take five waves,
  at most 50 seconds of fetching; the 60-second deadline is authoritative.

Extraction and matching (`research.Document`):
- HTML is parsed with `html.parser`, in 256 KiB pieces with the deadline checked between them; text inside
  `script`, `style`, `template`, `svg`, `noscript` and similar elements is skipped, and so is text inside any
  element hidden by its own attributes (`hidden`, `aria-hidden="true"`, an inline `display:none` or
  `visibility:hidden`), so a page cannot pass off text its readers never see; hiding through stylesheets or classes
  cannot be seen without running the page's CSS, so such text still counts as visible; block elements (paragraphs, list items, table cells, headings and the like) end a passage;
  plain text is split at blank lines; markup is parsed, never followed or run;
- normalisation is conservative: runs of whitespace become one space, typographic quotes and dashes become ASCII,
  the ellipsis character becomes three dots, invisible formatting characters are removed; case, accents, digits and
  other punctuation are kept;
- a quote matches only inside one passage, never across two; an exact match is preferred; a match that differs
  only in case is accepted and flagged in the evidence and the report, because case can change technical meaning;
- the trace keeps the page's own text for each match, its passage, source line, offset and length in the extracted
  text, and the extracted text's SHA-256.

Levels:

| Level | When | Used |
|---|---|---|
| verified | a claim's quote was found on that source's final page | cited evidence |
| not checkable | a PDF (the standard library cannot extract its text), HTTP 401, 402, 403, 407, 429 or 451, a page with under 40 characters of extractable text, an unsupported type or content encoding, more than 3 redirects | listed as a limitation, never cited |
| dropped | unreachable (an error or another status), blocked address or URL, oversized or malformed body, over the time limit, no claim's quote on the page, an identifier mismatch | listed with the reason |

A quote found on one source's page never verifies another source. SEO copies can quote accurately, so a match
confers no authority: it only shows the text was on the page.

Identifiers: DOIs and arXiv ids named in a verified source's URL or claims are looked up through the same guarded
fetcher, at most 10 per job: DOIs at `https://doi.org/` with CSL JSON content negotiation (Crossref and DataCite
answer with the work's metadata), arXiv ids at `https://arxiv.org/abs/` (the `citation_title` tag). The title is
compared with the title the search call gave and the page's own title (the same words, a prefix of three words or
more, or 80% shared words). A title that names another work, or an identifier the resolver does not know (HTTP 404
or 410), drops the source when the identifier is in its URL and the claim when it is in the claim. A resolver that
cannot be reached confirms nothing and drops nothing; the source list says the identifier is not confirmed. Resolver
success establishes identity, not quotation support, scientific quality or version equivalence.

Source signals (`research.signals`), from the final URL and page metadata only, shown in the source list:

| Signal | Rank |
|---|---|
| standards body (rfc-editor.org, ietf.org, w3.org, whatwg.org, iso.org, nist.gov, ...) | 0 |
| resolver-confirmed DOI or arXiv id; scholarly repository (arxiv.org, doi.org, PubMed, Zenodo, ...); scholarly metadata on the page (`citation_title`, `citation_doi`) | 1 |
| government or academic domain (`.gov`, `.edu`, `.ac.uk`, `europa.eu`, ...) | 2 |
| documentation (a `docs.` or `developer.` host, or a `/docs` or `/reference` path) | 3 |
| unclassified | 4 |
| aggregator or user-generated site (Wikipedia, Medium, Reddit, Stack Overflow, ResearchGate, ...), whatever else it shows | 5 |

The signals are preference rules, not proof: a domain suffix does not make a page right, and an unknown source stays
eligible. Evidence is ordered by rank, so the writer sees primary material first and is told to prefer it.

Works: verified sources with the same final or canonical URL (ignoring scheme, `www.`, fragment, tracking
parameters and a trailing slash), the same content hash or the same confirmed identifier are one work: one citation
number, its best-ranked address first and the others listed as "also at". A quote verified on one of the other
addresses says so.

## The write call and the report

The writer sees the brief and the verified evidence packets, framed as untrusted data: id, work, title, URL,
signals, date, claim, the quote as found on the page and whether it matched only ignoring case. It replies with one
JSON object:

```json
{"blocks": [{"type": "paragraph", "text": "...", "evidence": ["E1", "E3"]},
            {"type": "heading", "text": "..."}, {"type": "bullet", "text": "...", "evidence": ["E2"]},
            {"type": "gap", "text": "what the evidence does not cover"}]}
```

Paragraphs and bullets cite 1 to 8 verified ids; headings and gaps cite none; text is plain, with no URLs, links,
HTML, citation numbers or evidence ids; at most 60 blocks. Anything else rejects the reply, and the verified evidence
list is written instead; no repair call is made. Code then numbers the citations in order of first use, renders the
blocks as Markdown (escaped, so page or model text cannot open a link or start a block), lists the cited sources
with each quote, and lists what was not used: uncited verified evidence, sources that could not be checked and
dropped sources with their reasons. The final check confirms that every rendered citation number maps to a cited
source and every cited id to verified evidence.

What this check cannot do: it does not establish that a sentence follows from its quotes (entailment), that a
source is true, or that the writer used no outside knowledge. It narrows what the writer can cite, not what it can
say. Review claim-to-evidence support before relying on a report; for high-stakes use, review every citation.

## Privacy

The brief is sent to the search call, and from there to web searches. A brief that holds a file path, a fenced code
block, something that looks like a credential, an email address or a marker (CONFIDENTIAL, PROPRIETARY, INTERNAL
ONLY, DO NOT SHARE, NOT FOR DISTRIBUTION, UNDER NDA, a line reading PRIVATE, `[private]`), or that is over 20,000
characters, is refused before any call unless `--allow-private-search` is given; the refusal names the kind of
material and its line, not the material. This is a heuristic: it can miss private material and flag public text.
Web addresses are ignored when looking for file paths. Fetched pages, the brief and both replies stay in the trace,
which must be outside the repository and is created with mode 0700.

In the report, only an address that was fetched over http(s) is a link; an address that was dropped or never
fetched, whatever its scheme, is shown as plain text.

## Trace

`material.txt` (the brief), `backends.json`, `implementation.json`, `trace.jsonl` (the two calls, as for review and
design, and one `verify.S<n>` record per source with status `checked`, its level, reason, original and final URL,
redirect chain with each hop's address and status, retrieval time, content hash, byte counts and matched spans),
`steps/` (each reply), `sources.json` (the parsed search reply), `verification.json` (every record and identifier
lookup), `evidence.json` (the packets the writer saw), `result.md`, `summary.json` and `state.json`. The shell's
trace reader counts the calls and ignores the `checked` records.

## Not done

From the design (`research-router-2026-10-29`): the Rust runtime, the shell's route suggestions and its one active
research job per shell are not built; research is a Python reference-runtime command, and an explicit command is
the only way to run it. Under a native release, `python3 ringo.py research` runs the bundle's Python runtime; a
release staged before research existed says so and stops. The text of PDFs is not extracted, so PDFs are never
verified. The number of searches inside the gather turn is counted but cannot be capped. Live search has not been
exercised in a real model run, and research has not been evaluated against `ask` or a manually sourced baseline;
until it is, there is no claim of benefit.
