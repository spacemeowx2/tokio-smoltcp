# 0.2.0

* Upgrade `smoltcp` to v0.8
* Remove `FutureDevice`
* `Net::new()` now returns `Net`, don't need to spawn the `Future`.
* Move helpers in `util` to `device` module.
