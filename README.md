# mousekeys
mousekeys daemon for linux (written in Rust)

---

A highly configurable mousekeys daemon.

It has somewhat similar functionality to the Windows mousekeys daemon but is slightly different.

It has three modes, depending on the modifier in use (CTRL key held down (fast, linear movement), SHIFT key held down (slow, linear movement), or no key held down (movement acceleration with exponential smoothing).

All three modes' speeds and the default mode's acceleration is tweakable in the conf (must be in the same directory as the binary).

See the .conf for other options, and also see the header of the source code for more details of how it works (main.rs).

---

Must be run as root: sudo ./mousekeys &

---

WARNING: Not heavily tested, so use at your own risk! (It is multithreaded, so there is a chance there may be a bug somewhere that causes a lockup while processing xinput events.)
WARNING: Not heavily tested, so use at your own risk! (It is multithreaded, so there is a chance there may be a bug somewhere that causes a lockup while processing xinput events.)
WARNING: Not heavily tested, so use at your own risk! (It is multithreaded, so there is a chance there may be a bug somewhere that causes a lockup while processing xinput events.)

---

Edit the mousekeys.conf (must be in the same directory as the binary) to tweak the acceleration and speed.

Run `cargo build --release` to build it.

Copy the the config/mousekeys.conf to the target/release/ and edit it (it has a fairly aggressive setup as the default).

`cd target/releast`

Run `sudo ./mousekeys &` to launch it as a daemon. Can be kept running interactively to see information about what keyboard it is (prioritizing) using.

---

Written by Claude (Anthropic), ChatGPT (OpenAI), and myself.
