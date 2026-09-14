int archive_helper_lto_qld(int);

int archive_used_lto_qld(int x) { return archive_helper_lto_qld(x) + 1; }
