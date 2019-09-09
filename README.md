# Learning non-blocking IO in Rust

Exploring various ways of doing non-blocking IO in Rust by writing http servers.

Contents:
* `src/bin/http_mio_st.rs` - uses a hand-written single-threaded event loop driven by `mio::Poll`.
* `src/bin/http_mio_mt.rs` - uses a hand-written event loop driven by `mio::Poll` with tasks executed in multiple threads. This server has the best performance.
* `src/bin/http_tokio.rs` - uses tokio and futures v0.1
* `src/bin/http_tokio_async_await.rs` - uses tokio preview and async-await.
* `src/bin/http_minitokio.rs` - uses a tokio-like multithreaded runtime for executing async-await futures. See `src/minitokio.rs`.

Also, for comparison, contains:
* `src/bin/http_blocking_st.rs` - a single-threaded blocking server.
* `src/bin/http_blocking_mt.rs` - a thread-per-connection blocking server.
* `src/bin/http_blocking_tp.rs` - a blocking server that uses several threads that all accept connections from a single listening socket to balance load.
