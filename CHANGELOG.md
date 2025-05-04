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
