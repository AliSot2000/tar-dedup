# sparse-cp

Sparse-aware file copy and zero-block scanner. It copies files while
preserving/reconstructing holes and counts zero-block pages, which the
`tar-dedup` archive pipeline uses to decide on sparsification.

Part of the [`tar-dedup`](https://github.com/AliSot2000/tar-dedup) project.

## Build

```
cargo build -p sparse-cp --release
```