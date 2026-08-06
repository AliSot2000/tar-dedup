hole-detection -> Capability to be added later
sparse version (0.0, 0.1, and 1.0.) - Do we need to support different versions or only 1.0?

--null / --no-null needs to be considered carefully if we want this ability or not. (read stdin vs read file.) In any case, our first step would be to append the files to db.

Archive format selection
=> Ignore everything. We haver on format we use and one format we read. Nothing else. If we change that I need to modify my version. But the underlying tar ustar with gnu header extension is safe and sound and we can safely rely on this.
