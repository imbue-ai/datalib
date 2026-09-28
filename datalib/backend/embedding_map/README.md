# The embedding map

The map card (`umapView()`) shows every document qmd has embedded as a
point on a plane, placed so that documents with similar content sit near
each other. This crate computes where the points go. It is pure
computation: the `embedding_map` function in `datalib-step`
(`datalib_step/src/embedding_map.rs`) reads the vectors and writes the
result, and the `unified_index` applet serves it joined to the grid.

```
unified_index/qmd_index/qmd/index.sqlite     qmd's chunk vectors
        │  datalib_unified_index::qmd::vectors — one unit vector per document
        ▼
datalib_embedding_map::layout                 this crate
        │
        ▼
unified_index/embedding_map/embedding_map.json   path → (x, y), one file
        │  applets/src/unified_index/map.rs — joined to grid_rows by path
        ▼
the map card                                  datalib/ui/src/cards/UmapCard.ce.vue
```

## What happens in one run

1. **One vector per document.** qmd embeds each document in chunks and
   stores the vectors unnormalised. Each chunk is scaled to unit length,
   a document's chunks are averaged, and the average is scaled again.
   How the vectors are read out of qmd's SQLite file without the
   sqlite-vec extension is written up at the top of
   `unified_index/src/qmd/vectors.rs`.
2. **Nearest neighbours.** Each document's 15 nearest by cosine distance
   (fewer in a smaller corpus), exactly, by brute force: one matrix
   product per block of 256 documents. That is 1.7s for 18.7k documents
   on a laptop; it grows with the square of the corpus, so a corpus ten
   times larger wants an approximate index here instead.
3. **A starting layout, seeded from the last map.** A document the last
   map placed starts where it was. A new one starts at the mean of the
   places its neighbours had, spreading outward a ring at a time. What
   is left starts on the corpus's first two principal axes, carried into
   the old map's frame. A first run starts every point on the principal
   axes.
4. **UMAP.** `umap-rs` builds the fuzzy neighbourhood graph and fits the
   curve; the optimiser is ours (below). A seeded run takes 200 epochs
   at a tenth of the usual step size, so the layout settles what moved
   rather than shuffling what did not.
5. **A fit back onto the old frame.** The rotation, flip, scale and shift
   that best carry the new positions of the old documents onto their
   old positions (Procrustes), applied to every point. Whatever the run
   did to the map as a whole is taken out.

Measured on a real root of 18.7k documents: a seeded rerun moves the
median document 0.4% of the map's width, and 5% new documents arriving
leaves the rest where they were to the same 0.4%. The unit test
`documents_already_mapped_stay_put_when_more_arrive` fails with either
the seeding or the final fit taken out.

A **reset** of the step (the Manage row's menu, or the card's "Lay out
afresh") deletes the file, and the next run starts from nothing.

## Why the optimiser is ours

`umap-rs` 0.4.5's optimiser differs from umap-learn's
`optimize_layout_euclidean` in two ways that together stop clusters from
forming (checked against three well-separated synthetic clusters, which
it scattered):

- both gradients divide by `a·d^(2b)·d² + 1`, where the derivative of
  the curve it fits has `a·d^(2b) + 1`;
- `Optimizer::step_epochs` copies the embedding when it is called and
  reads the far end of every edge from, and moves it in, that copy — so
  within a call every point chases where its neighbours used to be.

`optimize` here is umap-learn's loop, on one thread with a fixed seed,
which also makes a layout a pure function of its inputs: the same
corpus from the same starting map always lays out the same way. The graph construction and
curve fit we still take from `umap-rs`; they match umap-learn.

## Colours

The card colours by one field with the eight categorical slots validated
for the app's light and dark surfaces, folding anything past seven
categories into a grey Other. A scatter plot puts every pair of colours
next to each other somewhere, which no eight-colour palette survives
under colour blindness, so a category is never told by colour alone:
the legend names it, and hovering a legend entry isolates it.
