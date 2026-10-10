## 1. Summary

- **Payloads are JSON objects, filtering is typed and usually existential over arrays**, missing, null and empty arrays have distinct behavior, while nested filters bind multiple predicates to the same array object, ordinary flattened predicates do not. `Payload`, `JsonPath`, `Filter` and `Condition` define these semantics. (`lib/segment/src/types.rs:2490`, `lib/segment/src/types.rs:2523`, `lib/segment/src/payload_storage/query_checker.rs:151`)
- **Payload indexes provide candidate retrieval, cardinality estimates and direct condition checks**, they also affect filtered vector-search planning, payload ordering, facets and physical point clustering. Integer lookup and range are separate index structures, every indexed field also receives a null/missing support index. (`lib/segment/src/index/field_index/index_selector.rs:142`, `lib/segment/src/index/struct_payload_index/build.rs:14`, `lib/segment/src/index/struct_payload_index/read_view/payload_index_read.rs:173`)
- **The universal Query API is a candidate-generation and rescoring tree**, leaf operations search vectors, scroll IDs, order payload values or sample points, while parent operations restrict further work to prefetched candidates. Root fusion and MMR execute across merged shard results, nested fusion executes within shards. (`lib/shard/src/query/planned_query.rs:242`, `lib/shard/src/query/planned_query.rs:327`, `lib/collection/src/collection/query.rs:352`)
- **Fusion and exploratory scoring have implementation-specific mathematics**, default RRF uses zero-based ranks with \(k=2\), weighted RRF changes the rank denominator, DBSF uses sample standard deviation, recommendation/discovery use bounded rational sigmoid functions, and MMR returns selection order with original relevance scores. (`lib/segment/src/common/reciprocal_rank_fusion.rs:14`, `lib/segment/src/common/score_fusion.rs:126`, `lib/segment/src/vector_storage/query/reco_query.rs:67`, `lib/shard/src/query/mmr/mod.rs:197`)
- **Local BM25 embedding is available in the server source, managed neural inference is a separate service**, payload full-text filtering and BM25 sparse ranking are different mechanisms. Important compatibility risks include empty-`should` evaluator disagreement, scan/index text differences, bounded grouping, array-valued ordering and candidate-dependent fusion. (`src/common/inference/local_model.rs:12`, `lib/bm25/src/lib.rs:142`, `lib/segment/src/data_types/index.rs:484`, `lib/segment/src/payload_storage/query_checker.rs:58`, `lib/segment/src/index/struct_payload_index/read_view/optimizer.rs:168`)

This report uses static source inspection, no project code was built or executed.
Engine paths are relative to `qdrant/`, documentation paths are relative to `landing_page/qdrant-landing/content/`, the supplied engine identifies itself as version `1.19.2`. (`Cargo.toml:3`)

## 2. How it works, fundamentally

### Payload representation and array semantics

`Payload` wraps `serde_json::Map<String, Value>`, the root is an object containing arbitrary JSON values, an index declaration does not make the entire payload a rigid typed record. `PayloadContainer::get_value` returns multiple references through a `JsonPath`, missing paths return an empty collection. Paths contain object keys, explicit array indexes and wildcard array indexes such as `items[]`, traversal does not automatically descend through every array. (`lib/segment/src/types.rs:2490`, `lib/segment/src/types.rs:2523`, `lib/segment/src/json_path/mod.rs:19`, `lib/segment/src/json_path/mod.rs:320`)

Field evaluation and indexing flatten one array level, a scalar predicate normally succeeds when any compatible array element satisfies it, unsupported values contribute no entries to a typed index. Consequently, an array containing strings, numbers and nulls can participate differently in keyword, numeric and null indexes, recursively nested arrays are not recursively flattened by the ordinary value indexer. (`lib/segment/src/payload_storage/condition_checker.rs:82`, `lib/segment/src/index/field_index/field_index_base/value_indexer.rs:20`)

Payload mutation is significant for API compatibility: `merge_map` replaces individual top-level values rather than recursively merging objects, and an incoming null removes that key. `merge_by_key` applies a merge at a selected path, overwrite persists the supplied object through the storage implementation, so null in an overwrite and null in a merge have different consequences. (`lib/segment/src/common/utils.rs:44`, `lib/segment/src/types.rs:2525`, `lib/segment/src/payload_storage/payload_storage_impl.rs:199`)

`is_empty` accepts a missing path, null or an empty array, an empty string and empty object remain values. `is_null` requires an actual null, including a null element in a directly inspected array, missing alone does not satisfy it. `values_count` treats missing/null as zero, an array as its length and a scalar/object as one. (`lib/segment/src/common/utils.rs:20`, `lib/segment/src/types.rs:3472`, `lib/segment/src/payload_storage/condition_checker.rs:208`)

### Index families and storage

`StructPayloadIndex` owns field indexes and schema configuration, plus references to payload storage, the point-ID tracker and named vector storages. `FieldIndex` is an enum over specialized implementations, the important interfaces are `ValueIndexer`, payload-index read operations and the payload-storage traits. (`lib/segment/src/index/struct_payload_index/mod.rs:63`, `lib/segment/src/index/field_index/field_index_base/field_index.rs:28`, `lib/segment/src/payload_storage/payload_storage_base.rs:19`)

| Declared type | Representation and supported operations |
|---|---|
| `keyword` | `MapIndex<str>`, value-to-point postings and reverse point-to-values mapping, exact match, any/except and facets, optional sorted keys for prefix lookup. (`lib/segment/src/index/field_index/map_index/mutable_map_index/in_memory.rs:21`) |
| `integer` | `MapIndex<i64>` for lookup and `NumericIndex<i64, i64>` for range/order operations, either or both can be built. (`lib/segment/src/index/field_index/index_selector.rs:151`) |
| `float` | `NumericIndex<f64, f64>`, ordered numeric pairs, histogram estimates and range/payload-order operations. (`lib/segment/src/index/field_index/field_index_base/field_index.rs:28`, `lib/segment/src/index/field_index/numeric_index/mutable_numeric_index/mod.rs:28`) |
| `bool` | Separate true/false `RoaringFlags` structures, a point with both values in an array can inhabit both sets, facets are supported. (`lib/segment/src/index/field_index/bool_index/mutable_bool_index/mod.rs:13`, `lib/segment/src/index/field_index/field_index_base/field_index_read_impl.rs:290`) |
| `geo` | Geohash counts/postings plus exact per-point coordinates, geometric checking determines the actual predicate result. (`lib/segment/src/index/field_index/geo_index/on_disk_geo_index/mod.rs:77`, `lib/segment/src/types.rs:3524`) |
| `datetime` | Parsed UTC timestamps indexed as signed microseconds through `NumericIndex<i64, DateTimePayloadType>`, range and ordering operate on that representation. (`lib/segment/src/index/field_index/numeric_index/value_indexer.rs:56`, `lib/segment/src/types.rs:79`) |
| `uuid` | Current selector builds `MapIndex<u128>`, compact UUID equality lookup and facets, the enum retains a legacy numeric UUID index variant. (`lib/segment/src/index/field_index/index_selector.rs:194`, `lib/segment/src/index/field_index/field_index_base/field_index.rs:28`) |
| `text` | Vocabulary, inverted token postings, reverse point/token data and optional positional documents for phrase matching. (`lib/segment/src/index/field_index/full_text_index/inverted_index/mutable_inverted_index.rs:14`) |

Payload objects are serialized as JSON bytes inside `Blobstore<Payload>`, the payload-storage directory is `payload_storage`, the default backing abstraction uses `MmapFile`. Mutable map indexes combine RAM lookup structures and a blobstore of per-point values, mutable numeric indexes combine a `BTreeSet` of value/point pairs, a histogram and reverse mappings. (`lib/segment/src/payload_storage/payload_storage_impl.rs:19`, `lib/segment/src/index/field_index/map_index/mutable_map_index/mod.rs:12`, `lib/segment/src/index/field_index/numeric_index/mutable_numeric_index/mod.rs:28`)

Disk-oriented map indexes use `values_to_points.bin` and `mmap_field_index_config.json`, numeric indexes use sorted `data.bin` pairs with an optional `data_block_index.bin`, geo indexes use geohash count/posting files and point-coordinate mappings. Immutable numeric and geo representations keep runtime deletion masks in memory and reconstruct them from authoritative point deletion state on reload, those masks are not independent durable truth. (`lib/segment/src/index/field_index/map_index/on_disk_map_index/mod.rs:17`, `lib/segment/src/index/field_index/numeric_index/on_disk_numeric_index/mod.rs:15`, `lib/segment/src/index/field_index/geo_index/on_disk_geo_index/mod.rs:16`)

Text disk files include `postings.dat`, `vocab.dat`, `point_to_tokens_count.dat` and optional document-length data, mutable text documents are stored as CBOR. Every indexed field additionally receives a `NullIndex` with presence/null flags, its initial point count includes points without payload, allowing missing-field queries to include them. (`lib/segment/src/index/field_index/full_text_index/inverted_index/on_disk_inverted_index/mod.rs:43`, `lib/segment/src/index/field_index/full_text_index/lifecycle.rs:166`, `lib/segment/src/index/field_index/null_index/mod.rs:70`, `lib/segment/src/index/struct_payload_index/build.rs:14`)

### Text processing

The tokenizers are whitespace, word, prefix and multilingual, word is the default and splits on non-alphanumeric characters. Prefix indexing emits prefixes of document tokens, query processing uses the query token’s selected/truncated prefix, multilingual processing uses Charabia and includes Japanese segmentation through Vaporetto. (`lib/segment/src/data_types/index.rs:271`, `lib/segment/src/index/field_index/full_text_index/tokenizers/mod.rs:21`, `lib/segment/src/index/field_index/full_text_index/tokenizers/mod.rs:57`, `lib/segment/src/index/field_index/full_text_index/tokenizers/multilingual.rs:22`)

Processing can fold ASCII, lowercase, remove stopwords, apply Snowball stemming and validate token length, stopwords support a language or a set of languages plus custom words. Text matching intersects all query-token postings, `text_any` unions them, an empty token query matches nothing, an unknown token prevents an all-token match while any-token matching drops unknown tokens. (`lib/segment/src/index/field_index/full_text_index/tokenizers/tokens_processor.rs:73`, `lib/segment/src/data_types/index.rs:356`, `lib/segment/src/data_types/index.rs:479`, `lib/segment/src/index/field_index/full_text_index/inverted_index/mutable_inverted_index.rs:84`, `lib/segment/src/index/field_index/full_text_index/full_text_index_read.rs:208`)

Phrase-enabled indexes preserve token order, a sentinel separates array string values so a phrase cannot cross two elements. Non-phrase mutable documents sort and deduplicate token IDs, indexed phrase matching returns no results when positional matching is disabled. Ordinary all-token matching combines indexed tokens from array elements, while the unindexed checker examines each scalar separately, creating a potential array-level scan/index discrepancy **(inferred)**. (`lib/segment/src/index/field_index/full_text_index/lifecycle.rs:122`, `lib/segment/src/index/field_index/full_text_index/lifecycle.rs:166`, `lib/segment/src/index/field_index/full_text_index/inverted_index/mutable_inverted_index.rs:128`, `lib/segment/src/payload_storage/condition_checker.rs:24`)

### Filter language

A filter combines its blocks with AND: `must` requires every condition, `should` requires at least one, `must_not` requires every condition to be false, and `min_should` requires at least `min_count` true conditions. A condition can contain another filter, allowing Boolean trees, API validation requires `min_count >= 1`. (`lib/segment/src/types.rs:4542`, `lib/segment/src/types.rs:4556`, `lib/segment/src/payload_storage/query_checker.rs:46`)

| Condition | Semantics |
|---|---|
| Exact `match`, `any`, `except` | Exact Boolean/string/integer comparisons, any is membership in a homogeneous string/integer set, except succeeds on a qualifying non-null value outside the set. Except on arrays is existential, it is not equivalent to negating any. (`lib/segment/src/payload_storage/condition_checker.rs:227`, `lib/segment/src/payload_storage/condition_checker.rs:275`, `lib/segment/src/payload_storage/condition_checker.rs:295`) |
| Numeric/datetime `range` | Optional `lt`, `gt`, `lte`, `gte` bounds combine conjunctively, numeric payload checks use floating conversion, datetime indexes use parsed timestamps. (`lib/segment/src/types.rs:3435`, `lib/segment/src/payload_storage/condition_checker.rs:324`, `lib/segment/src/index/field_index/numeric_index/value_indexer.rs:56`) |
| `text`, `text_any`, `phrase` | All processed tokens, any processed token, or ordered consecutive tokens respectively, behavior depends on index tokenization and phrase configuration. (`lib/segment/src/index/field_index/full_text_index/full_text_index_read.rs:208`) |
| `prefix`, `substring` | String prefix and substring checks are case-sensitive, prefix can use an enabled keyword dictionary, substring remains an expensive scan operation. (`lib/segment/src/types.rs:3129`, `lib/segment/src/types.rs:3141`, `lib/segment/src/payload_storage/condition_checker.rs:259`) |
| `is_empty`, `is_null`, `values_count` | Presence/null/array-size semantics described above, count bounds use the same four comparison operators. (`lib/segment/src/common/utils.rs:20`, `lib/segment/src/types.rs:3472`) |
| `has_id`, `has_vector` | External IDs resolve through the ID tracker, a named vector must exist and be non-deleted. (`lib/segment/src/payload_storage/query_checker.rs:151`) |
| `nested` | Automatically adds an array wildcard, the entire inner filter is evaluated against each individual object, one object must satisfy it. The checker supplies neither ordinary point-ID lookup nor named-vector storage inside that object scope. (`lib/segment/src/types.rs:4073`, `lib/segment/src/payload_storage/query_checker.rs:162`) |
| Geo box/radius/polygon | Box comparisons exclude boundaries and support longitude wrap across the antimeridian, radius uses Haversine meters with strict `< radius`, polygon supports a closed exterior and holes and uses `geo::Contains`. (`lib/segment/src/types.rs:3524`, `lib/segment/src/types.rs:3551`, `lib/segment/src/types.rs:3584`, `lib/segment/src/types.rs:3607`) |

A `FieldCondition` is not an implicit conjunction of every supplied subpredicate, its ordinary match/range/geo alternatives are checked disjunctively, while `values_count`, `is_empty` and `is_null` have precedence branches. Validation requires at least one predicate rather than exactly one, callers needing conjunction should put separate conditions in `must`. (`lib/segment/src/payload_storage/condition_checker.rs:129`, `lib/segment/src/types.rs:3886`)

### Cardinality estimation and execution planning

`CardinalityEstimation` carries minimum, expected and maximum matching point counts plus primary clauses capable of enumerating candidates, an unknown estimate is approximately \((0,N/2,N)\). Numeric indexes estimate through histograms and correct value counts for multiple values per point, ID/vector conditions can obtain exact local counts. (`lib/segment/src/index/field_index/mod.rs:64`, `lib/segment/src/index/field_index/numeric_index/query.rs:39`, `lib/segment/src/index/struct_payload_index/read_view/filtering.rs:121`)

For child expected counts \(e_i\), the estimator uses independence approximations:

\[
E[\text{AND}]=N\prod_i(e_i/N),\qquad
E[\text{OR}]=N\left(1-\prod_i(1-e_i/N)\right).
\]

AND bounds use the smallest upper bound and an overlap lower bound, OR bounds use the largest lower bound and summed upper bounds capped by \(N\), NOT swaps/complements the bounds. `min_should` enumerates combinations of the required number of clauses, estimates their intersections and unions them, impossible thresholds return zero. (`lib/segment/src/index/query_estimator.rs:113`, `lib/segment/src/index/query_estimator.rs:158`, `lib/segment/src/index/query_estimator.rs:189`, `lib/segment/src/index/query_estimator.rs:330`)

The optimizer compiles checker closures, preferentially reads index data instead of loading JSON, sorts `must` by increasing expected cardinality and `should` by decreasing cardinality for early exits. Candidate enumeration uses primary clauses and a visited pool for deduplication, it falls back to scanning when no usable clause exists, remaining conditions still need evaluation. (`lib/segment/src/index/struct_payload_index/read_view/optimizer.rs:29`, `lib/segment/src/index/struct_payload_index/read_view/optimizer.rs:180`, `lib/segment/src/index/struct_payload_index/read_view/optimizer.rs:233`, `lib/segment/src/index/struct_payload_index/read_view/payload_index_read.rs:173`)

Filtered HNSW dispatch adjusts estimates to the named vector’s availability, chooses a plain path when the maximum count is below the scan threshold, a graph path when the minimum exceeds it, and samples cardinality when bounds straddle it. Availability adjustment assumes missing vectors are independent of payload predicates, correlated missingness can therefore distort expected work **(inferred)**. (`lib/segment/src/index/hnsw_index/hnsw/read_view/dispatch.rs:94`, `lib/segment/src/index/query_estimator.rs:26`)

### Lifecycle, locks and durability

Segments are accessed through `Arc<RwLock<Segment>>` or proxy equivalents, payload/index internals also use `AtomicRefCell` references, these are not evidence of lock-free mutation. Updates use the segment holder’s conditional-move machinery, payload updates are chunked in groups of 32, filtered updates first resolve affected points. (`lib/shard/src/locked_segment.rs:32`, `lib/segment/src/index/struct_payload_index/mod.rs:63`, `lib/shard/src/update/payload.rs:14`, `lib/shard/src/segment_holder/mod.rs:879`)

Index creation scans stored payloads, builds the selected field structures and the null index, compatible persisted representations can be reloaded, incompatible configurations trigger rebuilds. Payload mutation writes storage, rereads the merged object and updates affected indexed paths, deletes/clears remove the associated index entries. (`lib/segment/src/index/struct_payload_index/build.rs:14`, `lib/segment/src/index/struct_payload_index/build.rs:74`, `lib/segment/src/index/struct_payload_index/payload_index.rs:140`, `lib/segment/src/index/struct_payload_index/payload_index.rs:162`)

Flush ordering is correctness-sensitive: field indexes flush before payload storage, dropped/cancelled indexes are handled so payload/version persistence can catch up, otherwise WAL replay filtering can silently omit points. The build code also warns that component flushing outside serialized shard flush capture/execution can double-free superseded storage blocks, rebuilding during live updates first flushes the whole segment through that serialization path. (`lib/segment/src/index/struct_payload_index/payload_index.rs:249`, `lib/segment/src/index/struct_payload_index/build.rs:26`)

### Universal Query API and prefetch trees

`QueryRequestInternal` accepts optional recursive prefetches, a query, vector name `using`, filter, search parameters, threshold, output selectors, limit, numeric offset and cross-collection `lookup_from`. Vector inputs can be raw vectors, point IDs or inference objects, query variants include nearest, recommend, discover, context, payload order, fusion, formula, random sampling and relevance feedback. (`lib/api/src/rest/schema.rs:557`, `lib/api/src/rest/schema.rs:570`, `lib/api/src/rest/schema.rs:637`)

`PlannedQuery` separates leaf searches/scrolls from `MergePlan` rescoring stages, parent filters propagate into child retrieval. Without a query, the leaf becomes an ID scroll, vector queries become core searches, `order_by` becomes ordered scroll and `sample: random` becomes random scroll, fusion/formula need prefetch candidates. Rescoring commonly constructs a `has_id` restriction over the union of child candidates, it cannot introduce points outside that union. (`lib/shard/src/query/planned_query.rs:17`, `lib/shard/src/query/planned_query.rs:327`, `lib/shard/src/query/planned_query.rs:408`, `lib/collection/src/shards/local_shard/query.rs:502`)

Root fusion and MMR are finalized after collection-level merging, nested fusion is materialized on each shard, other point-independent rescoring runs on shards. Child limits are explicit candidate budgets, root pagination does not automatically enlarge every child, the documentation warns that prefetch limits must cover `limit + offset`. (`lib/shard/src/query/planned_query.rs:242`, `lib/shard/src/query/planned_query.rs:327`, `lib/collection/src/collection/query.rs:352`, `documentation/search/hybrid-queries.md:30`)

### Scoring mathematics

Nearest-neighbor output depends on the configured metric, dot/cosine use larger-is-better, Euclidean/Manhattan distances use smaller-is-better. Point-ID examples resolve stored vectors, referenced example IDs are excluded through a generated `must_not has_id` filter. (`lib/segment/src/types.rs:314`, `lib/collection/src/operations/universal_query/collection_query.rs:230`, `lib/collection/src/operations/universal_query/collection_query.rs:522`)

For **RRF**, with zero-based rank \(r_j(p)\), source weight \(w_j>0\) and parameter \(k\):

\[
S(p)=\sum_{j:p\in L_j}
\frac{1}{(r_j(p)+1)/w_j+k-1}.
\]

Unit weights reduce this to \(1/(r+k)\), default \(k=2\) contributes \(1/2,1/3,1/4,\ldots\), non-positive weights contribute zero. Raw retrieval scores are ignored, contributions sum by point ID, weight count must match source count, equal fused scores have no explicit ID tie-break in the fusion sorter. This weighting is different from multiplying conventional \(1/(r+k)\) by \(w\). (`lib/segment/src/common/reciprocal_rank_fusion.rs:14`, `lib/segment/src/common/reciprocal_rank_fusion.rs:32`, `lib/segment/src/common/reciprocal_rank_fusion.rs:54`, `lib/shard/src/query/validation.rs:74`)

For **DBSF**, each result list independently computes mean \(\mu_j\) and sample standard deviation:

\[
\sigma_j^2=\frac{\sum_i(s_{ji}-\mu_j)^2}{n_j-1},\qquad
z_{ji}=\frac{s_{ji}-(\mu_j-3\sigma_j)}{6\sigma_j}
=\frac12+\frac{s_{ji}-\mu_j}{6\sigma_j}.
\]

Normalized contributions sum by ID, singleton/constant lists contribute \(0.5\), empty lists contribute nothing, values outside the three-standard-deviation interval are not clipped. DBSF therefore depends on candidate depth and the score distribution actually retrieved, root/global and nested/shard-local placement can change its output **(inferred)**. (`lib/segment/src/common/score_fusion.rs:18`, `lib/segment/src/common/score_fusion.rs:96`, `lib/segment/src/common/score_fusion.rs:126`)

Define \(f(x)=x/(1+|x|)\) and \(g(x)=\tfrac12(f(x)+1)\). Average recommendation creates \(\bar p\) without negatives or \(2\bar p-\bar n\) with negatives, dense/sparse examples are averaged, multidense examples instead concatenate positive vectors and negated negative vectors. Best-score recommendation compares maximum positive similarity \(a\) with maximum negative similarity \(b\), returning \(g(a)\) when \(a>b\), otherwise \(-g(b)\), ties favor the negative branch, sum-scores adds positive similarities and subtracts negative similarities. (`lib/common/common/src/math.rs:7`, `lib/segment/src/vector_storage/query/reco_query.rs:67`, `lib/segment/src/vector_storage/query/reco_query.rs:119`, `lib/segment/src/vector_storage/query/reco_query.rs:138`, `lib/segment/src/vector_storage/query/reco_query.rs:231`)

Discover combines a discrete context rank with target similarity:

\[
S_{\mathrm{discover}}(x)=
\sum_i \operatorname{sign}(s(x,p_i)-s(x,n_i))+g(s(x,t)).
\]

Context-only search instead sums bounded penalties:

\[
S_{\mathrm{context}}(x)=
\sum_i f\!\left(\min(s(x,p_i)-s(x,n_i)-\epsilon,0)\right).
\]

The first uses target relevance to refine context rank, the second has a near-zero plateau for satisfied pairs and increasingly negative penalties for violations. (`lib/segment/src/vector_storage/query/discover_query.rs:16`, `lib/segment/src/vector_storage/query/discover_query.rs:65`, `lib/segment/src/vector_storage/query/context_query.rs:53`, `lib/segment/src/vector_storage/query/context_query.rs:111`)

**MMR** first selects maximum query relevance, subsequent selections maximize:

\[
\lambda R(q,d)-(1-\lambda)\max_{s\in S}R(d,s),
\qquad \lambda=1-\mathrm{diversity}.
\]

Candidates are deduplicated, missing named vectors are discarded, pairwise similarities are cached lazily, output retains original relevance scores in greedy selection order. Root MMR selects enough points for `limit + offset` before pagination, it cannot diversify beyond its candidate pool. (`lib/shard/src/query/mmr/mod.rs:41`, `lib/shard/src/query/mmr/mod.rs:197`, `lib/collection/src/collection/query.rs:399`, `lib/collection/src/operations/universal_query/collection_query.rs:481`)

### Formula rescoring and boosting

`FormulaInternal` contains an expression and defaults, its AST supports constants, payload variables, condition indicators, `$score`/`$score[n]`, geographic distance, datetime conversion, arithmetic, min/max, powers, roots, logarithms and linear/exponential/Gaussian decays. Conditions evaluate to 0 or 1, missing source scores default to zero unless overridden, payload variables require compatible numeric values, missing-value defaults do not repair wrong types. Evaluation uses `f64`, final scores must remain finite after conversion to `f32`. (`lib/shard/src/query/formula.rs:14`, `lib/segment/src/index/query_optimization/rescore_formula/parsed_formula.rs:19`, `lib/segment/src/index/query_optimization/rescore_formula/parsed_formula.rs:288`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:78`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:99`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:324`)

Geo distance is Haversine meters, datetime values become UTC seconds, decay defaults are target 0, scale 1 and midpoint \(m=0.5\). With \(\delta=|x-t|\) and scale \(a\), the implementations are:

\[
D_{\mathrm{linear}}=\max(0,1-(1-m)\delta/a),\quad
D_{\mathrm{exp}}=m^{\delta/a},\quad
D_{\mathrm{gauss}}=m^{(\delta/a)^2}.
\]

Scale must be positive, exponential/Gaussian midpoint lies strictly between zero and one. Multiplication short-circuits at zero, division returns zero immediately for a zero numerator, so `0/0` can return zero without evaluating the denominator. (`lib/segment/src/index/query_optimization/rescore_formula/parsed_formula.rs:193`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:19`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:123`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:156`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:188`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:356`)

### Grouping, enumeration and aggregation

Grouping accepts a JSON path, group count, group size and optional lookup, group IDs are strings or signed/unsigned integers. Arrays let one point join multiple groups, duplicate keys are removed, an invalid key value can cause the entire point to be ignored, groups are ranked by their best hit and retain their best `group_size` hits. The driver uses five collection attempts and five fill attempts, it can return fewer or underfilled groups despite additional qualifying points **(inferred)**. (`lib/collection/src/operations/universal_query/collection_query.rs:73`, `lib/segment/src/data_types/groups.rs:7`, `lib/shard/src/grouping/aggregator.rs:60`, `lib/shard/src/grouping/aggregator.rs:111`, `lib/shard/src/grouping/driver.rs:7`)

`with_lookup` retrieves payload/vector metadata from another collection using group IDs convertible to ordinary point IDs, arbitrary string group names are not arbitrary join keys, unconvertible values are skipped and a missing target collection errors. (`lib/collection/src/lookup/mod.rs:21`, `lib/collection/src/lookup/mod.rs:48`)

ID scroll fetches one extra result, sorts/deduplicates across shards and returns the first omitted ID as `next_page_offset`, resume is inclusive from that ID. Payload ordering requires a compatible numeric index, supports integer/float/datetime, defaults to ascending and uses inclusive `start_from`, a point with several numeric values can appear multiple times. Ordered scroll rejects an ID offset, does not provide the normal next-ID cursor and deduplicates `(order_value, point_id)` pairs rather than IDs alone. (`lib/collection/src/collection/point_ops.rs:349`, `lib/collection/src/collection/point_ops.rs:402`, `lib/collection/src/shards/local_shard/shard_ops.rs:273`, `lib/segment/src/segment/read_view/order_by.rs:28`, `lib/segment/src/data_types/order_by.rs:100`)

Count defaults to exact, the local exact implementation materializes matching point IDs and counts them, approximate count uses the payload estimate’s expected cardinality. Facets require keyword, integer lookup, UUID lookup or Boolean indexes, exact mode enumerates values and obtains deduplicated counts, approximate mode selects candidate values through sampling/oversampling. Collection-level facet requests increase each shard budget to at least `max(4 × limit, 128)` before final aggregation. (`lib/shard/src/count.rs:11`, `lib/collection/src/shards/local_shard/shard_ops.rs:402`, `lib/segment/src/index/field_index/field_index_base/field_index_read_impl.rs:290`, `lib/collection/src/shards/local_shard/facet.rs:96`, `lib/segment/src/segment/read_view/facet.rs:103`, `lib/collection/src/collection/facet.rs:21`)

Distance matrix first randomly samples points with the named vector, then queries each sampled point against the sampled ID set, removes self-results and returns directed neighbor scores as pairs or sparse row/column/value arrays. Fewer than two sampled points yields an empty result, it is neither a guaranteed symmetric full matrix nor necessarily exact nearest-neighbor computation **(inferred)**, “distance” can contain similarity scores. (`lib/collection/src/collection/distance_matrix.rs:64`, `lib/collection/src/collection/distance_matrix.rs:140`, `lib/collection/src/collection/distance_matrix.rs:259`)

Legacy `search_batch` and universal query batches expose multiple independent requests, single search already delegates to a one-element batch, batching shares execution machinery and can defer payload retrieval until final results. No collection-wide snapshot guarantee was established by these paths, shard reads are joined asynchronously **(inferred)**. Random query streams are merged using random Boolean choices between streams, this alone does not establish uniform collection-wide sampling **(inferred)**. (`src/actix/api/search_api.rs:90`, `lib/api/src/rest/schema.rs:624`, `lib/collection/src/collection/search.rs:38`, `lib/collection/src/collection/search.rs:55`, `lib/collection/src/collection/query.rs:633`)

### Server inference and BM25 document embeddings

Inference inputs are deduplicated by full object, a global service partitions local and remote models, local computation runs in blocking tasks, remote inference uses a configured HTTP endpoint and optional credentials. `bm25` and `qdrant/bm25` are local model aliases, accepted case-insensitively, this path only accepts text documents and does not require an external inference address. Inference input text is not automatically persisted as payload. (`src/common/inference/infer_processing.rs:24`, `src/common/inference/service.rs:137`, `src/common/inference/service.rs:193`, `src/common/inference/local_model.rs:12`, `src/common/inference/service.rs:379`, `documentation/inference/inference-api.md:51`)

`lib/bm25` emits sparse document weights for processed term frequency \(tf(t,d)\), processed document length \(|d|\) and configured average length \(A\):

\[
W(t,d)=\frac{tf(t,d)(k_1+1)}
{tf(t,d)+k_1(1-b+b|d|/A)}.
\]

Query embeddings contain unique term IDs with weights one, repeated query terms do not increase query weight, sparse IDF processing supplies:

\[
IDF(t)=\ln\left(1+\frac{N-df(t)+0.5}{df(t)+0.5}\right).
\]

With the sparse IDF modifier enabled, dot-product ranking therefore sums \(IDF(t)W(t,d)\) over query/document terms, IDF can use the global corpus or an explicit corpus filter independent of the retrieval filter. The configured average length is baked into document weights, it is not automatically replaced by a changing collection average. (`lib/bm25/src/lib.rs:118`, `lib/bm25/src/lib.rs:142`, `lib/segment/src/data_types/query_context.rs:92`, `lib/segment/src/data_types/query_context.rs:353`, `lib/segment/src/types.rs:685`)

Terms use Murmur3 seed zero followed by signed-integer absolute conversion, collisions merge query IDs and can overwrite a document term’s entry during map insertion, this is a correctness tradeoff rather than a collision-free vocabulary **(inferred)**. Payload text indexes have document-length/scoring groundwork, but `TEXT_INDEX_SCORING` is false in production, they should not be described as the active BM25 ranking interface. (`lib/bm25/src/lib.rs:142`, `lib/bm25/src/lib.rs:166`, `lib/segment/src/data_types/index.rs:484`)

## 3. Config knobs and defaults that matter

| Area | Defaults and meaningful controls |
|---|---|
| Query outputs/pagination | Query and prefetch limit 10, query offset 0, payload/vector output false, scroll limit 10 with payload true and vectors false, group size 3. Query/prefetch limits validate at least one. (`lib/api/src/rest/schema.rs:570`, `lib/api/src/rest/schema.rs:776`, `lib/shard/src/scroll.rs:11`, `lib/collection/src/operations/universal_query/collection_query.rs:34`) |
| Integer index | `lookup=true`, `range=true`, disabling both is rejected, range-only loses equality/facet lookup structure, lookup-only lacks numeric ordering structure. (`lib/segment/src/data_types/index.rs:68`, `lib/segment/src/data_types/index.rs:118`) |
| Memory placement | Field-index default is pinned, explicit `memory` overrides legacy `on_disk`, cold pages load lazily, cached pages are populated but evictable, pinned field structures use heap memory. (`lib/segment/src/types.rs:1878`, `lib/segment/src/data_types/index.rs:26`) |
| Clustering flags | Keyword/UUID `is_tenant=false`, integer/float/datetime `is_principal=false`, both categories feed the physical tenant-optimization hint used for defragmentation. They are not tenant isolation or authorization controls **(inferred)**. (`lib/segment/src/types.rs:2714`, `lib/shard/src/optimize.rs:234`, `lib/segment/src/segment_constructor/segment_builder.rs:329`) |
| Graph interaction | Index `enable_hnsw` defaults true where exposed, extra payload graph edges require nonzero `payload_m`. HNSW defaults include `m=16`, `ef_construct=100`, full-scan threshold 10,000 KB, converted to point counts using vector size. (`lib/segment/src/data_types/index.rs:26`, `lib/segment/src/types.rs:1645`, `lib/segment/src/types.rs:2361`, `lib/segment/src/index/hnsw_index/hnsw.rs:197`) |
| Text | Word tokenizer, lowercase true, ASCII folding false, no default min/max token length, no stopwords or stemmer, phrase matching false, keyword prefix support false. (`lib/segment/src/data_types/index.rs:26`, `lib/segment/src/data_types/index.rs:271`, `lib/segment/src/data_types/index.rs:281`) |
| Fusion/MMR | RRF \(k=2\), custom \(k\ge1\), default weights one, MMR diversity 0.5, candidate limit defaults to requested result limit and has maximum 16,384. A larger candidate pool must be requested explicitly for meaningful reranking headroom. (`lib/segment/src/common/reciprocal_rank_fusion.rs:14`, `lib/api/src/rest/schema.rs:544`, `lib/api/src/rest/schema.rs:756`) |
| Count/facet/matrix | Count exact true, facet exact false and limit 10, distance sample 10 with minimum two and neighbor limit three with minimum one. (`lib/shard/src/count.rs:11`, `lib/api/src/rest/schema.rs:1318`, `lib/api/src/rest/schema.rs:1383`) |
| BM25/inference | \(k_1=1.2\), \(b=0.75\), average length 256, word tokenizer, English language processing, lowercase true, ASCII folding false, default English stopwords/stemming unless overridden, inference timeout defaults to 600 seconds. (`lib/api/src/rest/schema.rs:182`, `src/common/inference/bm25_inference.rs:62`, `src/common/inference/service.rs:75`) |
| Search/budgets | `exact=false`, `indexed_only=false`, optional HNSW `ef`, recursive prefetch maximum depth 64, strict mode can restrict query limit, timeout, batch size, filter conditions and membership-set size. (`lib/segment/src/types.rs:656`, `lib/shard/src/query/planned_query.rs:12`, `lib/segment/src/types.rs:1325`) |

Pagination work is approximately top-`limit + offset`, the planner uses saturating addition but does not make large offsets cheap, there is no single unconditional small maximum across all query limits in the inspected schema, strict-mode limits supply configurable ceilings. Prefetch depth is bounded, depth alone does not bound tree width or total candidate work **(inferred)**. (`lib/shard/src/query/planned_query.rs:110`, `lib/segment/src/types.rs:1334`, `lib/segment/src/types.rs:1372`)

## 4. Edge cases, failure modes and known sharp edges

### Evaluator and documentation disagreement

The raw filter evaluator gives `should: []` false because `any` on an empty iterator is false, optimized filtering skips an empty `should` constraint and treats it as satisfied. This is a confirmed implementation disagreement from static inspection, its endpoint impact depends on which evaluator a request reaches, no runtime reproduction was performed. (`lib/segment/src/payload_storage/query_checker.rs:58`, `lib/segment/src/index/struct_payload_index/read_view/optimizer.rs:168`, `lib/segment/src/index/query_optimization/optimized_filter.rs:56`)

The filtering documentation describes unindexed text as whitespace-tokenized, case-sensitive substring matching, the source uses word tokenization, lowercase processing and token membership. Documentation also says phrase search requires a phrase-enabled text index, the raw checker can evaluate a phrase without an index, while an existing non-phrase index returns no phrase matches, adding an index can therefore change observed behavior **(inferred)**. (`documentation/search/filtering.md:238`, `documentation/search/text-search/text-filtering.md:120`, `lib/segment/src/payload_storage/condition_checker.rs:24`, `lib/segment/src/index/field_index/full_text_index/inverted_index/mutable_inverted_index.rs:128`)

The REST schema commentary describes best-score recommendation’s negative result using a squared score, the implementation uses the negative bounded sigmoid described above, trust the implementation. (`lib/api/src/rest/schema.rs:816`, `lib/segment/src/vector_storage/query/reco_query.rs:67`)

### Array, numeric and geometric traps

Flattened `must` predicates can match different objects within the same array, nested filters are required for same-object correlation. `except` can succeed because one element is outside the excluded set even when another is inside it, wrong-type non-null values can also satisfy its negative comparison branch, separate `must_not any` has different semantics. (`lib/segment/src/payload_storage/query_checker.rs:162`, `lib/segment/src/payload_storage/condition_checker.rs:295`)

Datetime parsing accepts RFC3339 plus several naive/date-only formats and assumes UTC for timezone-less values, this is broader than requiring explicit RFC3339 timestamps. Numeric scan checks and ordered-scroll start conversion use `f64`, integers above its exact-integer range can lose precision **(inferred)**, strict radius/box boundaries and polygon containment also deserve explicit compatibility tests. (`lib/segment/src/types.rs:79`, `lib/segment/src/payload_storage/condition_checker.rs:324`, `lib/segment/src/data_types/order_by.rs:119`, `lib/segment/src/types.rs:3524`, `lib/segment/src/types.rs:3584`)

### Ranking and candidate-budget traps

RRF/DBSF/formula/MMR cannot recover candidates omitted by leaf retrieval, large root offsets with shallow prefetches can yield empty pages. Nested fusion uses shard-local ranks/distributions, changing sharding can change nested fusion results **(inferred)**, root fusion performs its normalization/ranking over collection-merged lists. (`documentation/search/hybrid-queries.md:30`, `lib/shard/src/query/planned_query.rs:327`, `lib/collection/src/collection/query.rs:352`)

DBSF always normalizes increasing scores and sorts larger-is-better, while Euclidean/Manhattan nearest results expose smaller-is-better distances, directly combining such streams appears to reward worse distances unless another layer adjusts orientation **(inferred, unresolved)**. MMR result scores can decrease or increase out of numerical order because greedy selection order is retained. (`lib/segment/src/common/score_fusion.rs:18`, `lib/segment/src/types.rs:350`, `lib/collection/src/collection/query.rs:389`, `lib/shard/src/query/mmr/mod.rs:197`)

Formula evaluation can fail on missing/wrong-type payloads, mathematical domain errors, infinity or `f32` overflow, short-circuiting makes some errors expression-order dependent. `min_should` planning can require \(\binom{n}{m}\) combinations, lazy MMR can approach quadratic pairwise work, these are query-complexity concerns **(inferred)**. (`lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:78`, `lib/segment/src/index/query_optimization/rescore_formula/formula_scorer.rs:156`, `lib/segment/src/index/query_estimator.rs:158`, `lib/shard/src/query/mmr/mod.rs:197`)

### Aggregation, pagination and durability traps

Ordered scroll is a stream of indexed values rather than necessarily one row per point, resuming from an inclusive value needs exclusions or a composite cursor to avoid repeats **(inferred)**. Exact facets can issue filtered counting work per distinct value, exact count materializes IDs, grouping has bounded retries and approximate facet candidates can omit globally important values **(inferred)**. (`lib/segment/src/segment/read_view/order_by.rs:28`, `lib/collection/src/collection/point_ops.rs:402`, `lib/collection/src/shards/local_shard/facet.rs:96`, `lib/collection/src/shards/local_shard/shard_ops.rs:402`, `lib/shard/src/grouping/driver.rs:7`, `lib/collection/src/collection/facet.rs:21`)

`indexed_only` may omit recently uploaded data in large unindexed segments, approximate counts are unreliable during indexing, dynamic index changes must preserve flush/WAL ordering. The source’s explicit silent-omission and double-free warnings make this lifecycle a core correctness requirement for a new implementation. (`lib/segment/src/types.rs:656`, `lib/shard/src/count.rs:11`, `lib/segment/src/index/struct_payload_index/payload_index.rs:249`, `lib/segment/src/index/struct_payload_index/build.rs:26`)

## 5. Features that are Cloud-only, enterprise-only or not in the open-source build

The inspected payload indexes, Boolean/geo/text filtering, query planning, fusion, formulas, grouping, scrolling, counts, facets, distance matrices and local BM25 embedding all have implementations in the supplied source, no Cloud or enterprise gate was identified in these paths.
This is a scoped source finding rather than a claim about every commercial Qdrant feature. (`lib/segment/src/index/field_index/index_selector.rs:142`, `lib/shard/src/query/planned_query.rs:242`, `lib/collection/src/collection/facet.rs:21`, `src/common/inference/local_model.rs:12`)

Managed **Cloud Inference** supplies hosted models and Cloud-console model selection/enabling, the open-source server contains an HTTP adapter for a configured inference service and a local BM25 implementation, the inspected path does not implement the managed neural-model hosting platform. Client-side FastEmbed is another route described by the docs, distinct from server-hosted neural inference. (`documentation/inference/cloud-inference.md:10`, `documentation/inference/cloud-inference.md:52`, `src/common/inference/service.rs:193`)

Strict-mode machinery is open source even where documentation describes it as enabled by default in Cloud, its controls are not inherently Cloud-only.
Conversely, native payload full-text scoring groundwork is disabled in production, it should not be counted as an available ranking feature merely because document lengths and scoring-related fields exist. (`documentation/search/text-search/text-filtering.md:30`, `lib/segment/src/types.rs:1325`, `lib/segment/src/data_types/index.rs:484`)

## 6. Design implications for nocturnum

The following are design proposals **(inferred)**, they describe independent conceptual choices rather than source reuse.

### Minimal v1

1. **Specify one payload/filter semantics before optimizing**, use JSON objects, explicit key/index/wildcard paths, one-level array existential matching, same-object nested filters and separately defined missing/null/empty behavior, reject field conditions containing several predicate kinds. A single reference evaluator should define empty Boolean blocks and typed negative matches, every index-backed evaluator should satisfy the same contract. The source’s multiple evaluators and permissive `FieldCondition` make semantic drift observable. (`lib/segment/src/payload_storage/query_checker.rs:46`, `lib/segment/src/payload_storage/condition_checker.rs:129`, `lib/segment/src/index/query_optimization/optimized_filter.rs:56`)
2. **Implement a small typed index set**, keyword postings, integer equality/range, float range, Boolean bitmaps and presence/null bitmaps, use reverse per-point values to support replacement/deletion and exact verification. Start with RAM structures and an independently designed persisted representation, add cold/mapped variants after lifecycle behavior is stable. (`lib/segment/src/index/field_index/field_index_base/field_index.rs:28`, `lib/segment/src/index/field_index/null_index/mod.rs:70`)
3. **Expose nearest search, filtered ID scroll and exact count**, provide a stable ID cursor and deterministic score/ID tie-breaks, establish one internal higher-is-better scoring convention and convert output distances explicitly. Reserve payload ordering, group filling and distributed aggregation until their cursor/consistency contracts are specified. (`lib/segment/src/types.rs:314`, `lib/collection/src/collection/point_ops.rs:349`, `lib/segment/src/common/reciprocal_rank_fusion.rs:54`)
4. **Use a bounded planner**, retain minimum/expected/maximum cardinality plus a candidate-enumerator abstraction, begin with exact bitmap counts and conservative bounds, choose scanning versus filtered ANN through explicit cost estimates. Cap filter depth/width, membership sets, candidate counts and total work, implement `min_should` evaluation without enumerating every clause combination during planning. (`lib/segment/src/index/field_index/mod.rs:64`, `lib/segment/src/index/query_estimator.rs:158`, `lib/segment/src/index/hnsw_index/hnsw/read_view/dispatch.rs:94`)
5. **Make payload/index durability one coordinated lifecycle**, define a single writer or transactional generation boundary, publish consistent read views and advance WAL durability only after all affected structures are recoverable. Treat crash recovery, index rebuild/drop and cancellation as acceptance criteria, the Qdrant flush warnings identify concrete failure classes to avoid. (`lib/segment/src/index/struct_payload_index/payload_index.rs:249`, `lib/segment/src/index/struct_payload_index/build.rs:26`)

### Later retrieval and API phases

An initial retrieval extension can add sparse BM25, RRF and a flat multi-source candidate stage, keep text filtering explicitly separate from text ranking. Prefer a collision-free persisted vocabulary or a specified collision strategy, maintain document lengths and corpus statistics so average-length changes need not require silently incompatible embeddings, version preprocessing configurations alongside persisted data. This addresses the inspected BM25 hash and fixed-average behavior **(inferred)**. (`lib/bm25/src/lib.rs:142`, `lib/bm25/src/lib.rs:166`, `src/common/inference/bm25_inference.rs:62`)

A subsequent phase can add datetime/UUID/geo indexes, positional text, facets, grouping and ordered cursors, define whether arrays generate several ordered rows or one selected value. Exact versus approximate aggregation should be explicit in response metadata, bounded grouping should expose truncation rather than silently resemble exhaustive grouping **(inferred)**. (`lib/segment/src/segment/read_view/order_by.rs:28`, `lib/collection/src/shards/local_shard/facet.rs:96`, `lib/shard/src/grouping/driver.rs:7`)

Formula ASTs, recommendation/discovery, MMR and recursive prefetch trees belong after the candidate API is stable, require score orientation and candidate-budget visibility. For distributed execution, choose whether nested fusion is deliberately shard-local or invariant under shard partitioning, local/global stages should be explicit in the plan rather than an incidental tree-position rule **(inferred)**. (`lib/shard/src/query/planned_query.rs:242`, `lib/shard/src/query/planned_query.rs:327`, `lib/collection/src/collection/query.rs:352`)

Tenant/principal clustering and memory tiers can follow measured workloads, clustering should use a declared ordering policy and remain distinct from access control. Remote inference can remain an adapter boundary, a minimal database need not own model serving **(inferred)**. (`lib/shard/src/optimize.rs:234`, `lib/segment/src/segment_constructor/segment_builder.rs:329`, `src/common/inference/service.rs:193`)

### Candidate Rust crates

These are source-grounded candidates for independent evaluation **(inferred)**, their presence in Qdrant is evidence of role rather than a current comparative ranking.

| Role | Candidates and rationale |
|---|---|
| JSON/API types | `serde`, `serde_json`, `schemars`, typed request ASTs and payload objects, avoid using untagged request ambiguity as the semantics definition. (`lib/segment/Cargo.toml:60`, `lib/segment/src/types.rs:4556`) |
| Sets and ordered values | `roaring`, standard `BTreeMap`/`BTreeSet`, postings, ID sets and numeric ordering, `ordered-float` where a total-order wrapper is required. (`lib/segment/src/index/field_index/map_index/mutable_map_index/in_memory.rs:21`, `lib/segment/src/index/field_index/numeric_index/mutable_numeric_index/mod.rs:28`, `Cargo.toml:310`) |
| Dates and identifiers | `chrono`, `uuid`, define accepted formats, precision and canonicalization explicitly. (`Cargo.toml:220`, `Cargo.toml:258`, `lib/segment/src/types.rs:79`) |
| Runtime and mapped storage | `tokio`, `parking_lot`, `memmap2`, separate asynchronous orchestration from blocking index work and preserve a clear storage publication boundary. (`Cargo.toml:251`, `Cargo.toml:260`, `Cargo.toml:303`, `src/common/inference/service.rs:137`) |
| Geo and linguistic processing | `geo`, `geohash`, `charabia`, Snowball-compatible stemming through an independently selected `rust-stemmers` implementation, add these when the relevant semantics are committed. (`lib/segment/Cargo.toml:74`, `lib/segment/Cargo.toml:101`, `lib/segment/src/index/field_index/full_text_index/tokenizers/multilingual.rs:22`) |

## 7. Open questions

1. **What is the compatibility target for ambiguous filters?** Decide whether nocturnum rejects or defines empty `should`, mixed predicate fields, wrong-type `except`, cross-element text matching and unindexed phrases, reproducing every observed branch would preserve implementation inconsistencies **(inferred)**. (`lib/segment/src/payload_storage/query_checker.rs:58`, `lib/segment/src/payload_storage/condition_checker.rs:129`, `lib/segment/src/index/field_index/full_text_index/lifecycle.rs:122`)
2. **Should fusion be invariant under shard placement?** Root/global and nested/local stages have different rank/distribution inputs, DBSF’s smaller-is-better handling also needs verification before adopting its mathematical contract. (`lib/shard/src/query/planned_query.rs:327`, `lib/collection/src/collection/query.rs:352`, `lib/segment/src/common/score_fusion.rs:18`)
3. **What consistency and cursor contract will pagination provide?** Specify behavior under concurrent writes, composite `(value, ID)` ordering, multiple values per point and full-width integer comparisons, the inspected ID and payload-order paths expose different resumability guarantees. (`lib/collection/src/collection/point_ops.rs:349`, `lib/collection/src/collection/point_ops.rs:402`, `lib/segment/src/data_types/order_by.rs:119`)
4. **Will text ranking store reusable term statistics or preweighted sparse documents?** Choose vocabulary collision policy, dynamic average-length handling, scoped IDF semantics and preprocessing-version migration, these choices determine whether corpus changes require document re-embedding **(inferred)**. (`lib/bm25/src/lib.rs:142`, `lib/bm25/src/lib.rs:166`, `lib/segment/src/types.rs:685`)
5. **Which query-complexity limits are hard guarantees?** Set ceilings for Boolean trees, recursive prefetch width, candidate unions, MMR and formula evaluation as well as depth, the inspected depth/strict-mode controls do not alone bound every cost **(inferred)**. (`lib/shard/src/query/planned_query.rs:12`, `lib/segment/src/types.rs:1325`, `lib/segment/src/index/query_estimator.rs:158`)