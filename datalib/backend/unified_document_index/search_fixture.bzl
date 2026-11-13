"""What a rust_test linking `//datalib/backend/unified_document_index:search_fixture` needs.

The fixture's rendered tree, its grid index with the search index's tables,
its vectors and the embedding model, as `data`; and their runfile paths as
`env`.
"""

SEARCH_FIXTURE_DATA = [
    "//tests/fixtures:ingested/backend_index.sqlite",
    "//tests/fixtures:ingested/qmd.tar",
    "//tests/fixtures:ingested/search_index.sqlite",
    "//tests/fixtures:ingested/inference_cache.sqlite",
    "//third-party/qmd_models:embeddinggemma",
]

SEARCH_FIXTURE_ENV = {
    "SEARCH_FIXTURE_EMBED_MODEL_RLOC": "$(rlocationpath //third-party/qmd_models:embeddinggemma)",
    "SEARCH_FIXTURE_PLAIN_GRID_RLOC": "$(rlocationpath //tests/fixtures:ingested/backend_index.sqlite)",
    "SEARCH_FIXTURE_GRID_RLOC": "$(rlocationpath //tests/fixtures:ingested/search_index.sqlite)",
    "SEARCH_FIXTURE_RENDERED_RLOC": "$(rlocationpath //tests/fixtures:ingested/qmd.tar)",
    "SEARCH_FIXTURE_VECTORS_RLOC": "$(rlocationpath //tests/fixtures:ingested/inference_cache.sqlite)",
}
