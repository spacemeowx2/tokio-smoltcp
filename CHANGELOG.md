# 0.7.0

- Upgrade the re-exported `smoltcp` dependency to `0.14` (breaking dependency/API change).
- Replace deprecated Tokio FD registration with an owned duplicate; `AsyncCapture` now requires `AsFd` instead of `AsRawFd`, and Tokio 1.53.3 or newer.
- Replace deprecated atomic update APIs and require Rust 1.95 or newer.
- Fix lost reactor wakeups and stop requests when `Net` is dropped before the reactor starts (#18).
- Return an error from TCP flush if the connection closes with unacknowledged data (#19).
- Add `TcpConnectOptions`, `tcp_connect_with_options` and `tcp_connect_lazy_with_options` to select a TCP source port (#16).
- Add a bounded `pcap --smoke` example mode that checks DNS, HTTP, flush and TCP shutdown on a real network interface and exits with an error on failure.

# 0.6.0

- Upgrade `smoltcp` to `0.13`
- Add `AsyncCapture::new_with_interest` so callers can use selectable read-only fds
- Fix the `pcap` example on macOS by using `pcap_get_selectable_fd()`

# 0.5.2

- Add `tcp_connect_lazy` to obtain local endpoint before connection completes

# 0.5.1

- Fix dead-lock in TcpStream::connect

# 0.5.0

- Update dependencies to latest versions (smoltcp v0.12)

# 0.4.1

- Add `set_any_ip` and `any_ip`

# 0.4.0

- Upgrade `smoltcp` to `0.10.0`
- Remove neighbor cache filling at initialization (smoltcp removed the feature)

# 0.3.1

- fix: the fields of Neighbor should be public

# 0.3.0

- Upgrade `tokio-util` to `0.7.4`, `pcap` to `1.0.0`
- Allow routes to be updated at runtime
- Allow to fill neighbor cache at initialization

# 0.2.4

- Fix reactor not working after tcp connect.

# 0.2.3

- Fixing low performance in the receiving direction.

# 0.2.2

- impl AsyncDevice for `Box<dyn AsyncDevice>`.

# 0.2.1

- Avoid compile errors when feature `medium-ieee802154` is enaboed.

# 0.2.0

- Upgrade `smoltcp` to v0.8
- Remove `FutureDevice`
- `Net::new()` now returns `Net`, don't need to spawn the `Future`.
- Move helpers in `util` to `device` module.
