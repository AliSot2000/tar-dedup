sparse-file-generation on archive added. é

--null / --no-null added for file separation in case file names contain \n

Archive format selection
=> Ignore everything. We haver on format we use and one format we read. Nothing else. If we change that I need to modify my version. But the underlying tar ustar with gnu header extension is safe and sound and we can safely rely on this.

When running simple tasks like linking - use rayon. Perhaps it's a stupid idea but we have implemented it like tha for the momenmt.