//! Injected hook DLL entry point — detours user32 clipboard APIs and proxies them to the
//! host store over the named pipe set in `GLASS_CLIP_PIPE`. Implemented in Task 10.
