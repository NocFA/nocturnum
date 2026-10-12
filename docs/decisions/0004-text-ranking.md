# 0004: text ranking is BM25 counted when the query runs

Decided while building text search on 2026-10-12, an implementation choice within the v1 scope rather than an owner decision.

## What

- Text search ranks points by BM25 over one payload field, opted into with a payload index of kind `text`, and the client sends words, while the server does the tokenizing.
- A text index is derived data like every payload index (decision 0003), built in memory per segment from the payloads and never logged or synced, and points not yet in a segment are tokenized as each search runs.
- Each segment's index keeps its terms as strings, with each term's rows and counts, and each row's length in terms.
- The statistics BM25 needs, how many documents there are, how many hold each term and their average length, are counted over the live points of the search's view when the search runs, across the whole collection whatever the filter.
- A document is a point whose field holds at least one term, with an array of strings counting as one document, so points without the field or with no words in it add nothing to the statistics.
- `k1` and `b` are per-query options, defaulting to 1.2 and 0.75, since nothing stored depends on them.
- Tokens are runs of letters and digits in any script, lowercased, with no stemming and no stop words.

The code is `crates/nocturnum-index/src/text.rs` for the index and the maths, and `crates/nocturnum-query/src/collection/text.rs` for the search.

## Why

The research (`docs/research/a3-query.md`, "Server inference and BM25 document embeddings") found two problems with Qdrant's BM25.

Its terms are hashed to 32-bit ids, so two words can share an id and mix their counts, and the average document length is fixed when documents are embedded, so it goes stale as the corpus changes and fixing it means embedding every document again.

Keeping terms as strings rules out collisions, and counting the statistics at query time means a deleted or added document changes later scores straight away, with nothing to keep in step with the WAL.

Counting over the whole collection rather than the filtered points keeps a point's score the same whatever filter finds it, which is what a hybrid query fusing several searches expects.

## What it costs

- **Query time:** a search counts each query term's live postings in every segment and tokenizes the memtable's text, which grows with the corpus and the memtable, so a query is capped at 1,024 different words.
- **Startup time and memory:** as decision 0003, the indexes are rebuilt from payloads when a segment opens and live in RAM, roughly the size of the text's distinct words plus a few bytes per word per document.
- **Text has to be in the payload:** the index reads the field, so the text is stored with the point, which suits keeping a document's text beside its vector anyway.
- **Languages:** without stemming `run` and `running` are different terms, and scripts written without spaces between words, such as Japanese, give one token per run of characters, so better tokenizers are a later option on the index.
