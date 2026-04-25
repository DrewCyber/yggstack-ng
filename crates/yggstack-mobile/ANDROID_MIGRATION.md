# Android Migration Guide: Go AAR → Rust AAR

This document describes every change required to replace the Go-based yggstack AAR
(built with `gomobile`) with the Rust-based AAR (built with `cargo-ndk` + UniFFI).

---

## 1. Build System Changes

### 1.1 Remove the Go AAR

Delete (or stop importing) the Go-generated AAR from your project:

```groovy
// REMOVE from build.gradle
implementation files('libs/yggstack.aar')
// or
implementation 'com.example:yggstack-go:x.y.z'
```

### 1.2 Add the Rust AAR / `.so` files

The Rust library ships as raw `.so` files, one per ABI. Place them into your module's
`src/main/jniLibs/` directory following the standard Android ABI layout:

```
app/src/main/jniLibs/
    arm64-v8a/    libyggstack_mobile.so
    armeabi-v7a/  libyggstack_mobile.so
    x86/          libyggstack_mobile.so
    x86_64/       libyggstack_mobile.so
```

The `.so` files are produced by the build script at
`crates/yggstack-mobile/scripts/build_android.sh`.

### 1.3 Add the UniFFI-generated Kotlin bindings

Run the binding generator (done automatically by `build_android.sh`) or copy the
pre-generated file to your source tree:

```
android-build/kotlin/yggstack_mobile/
    yggstack_mobile.kt      ← generated UniFFI Kotlin bindings
```

Add the generated file to your module's `src/main/java/` (or `src/main/kotlin/`) tree,
or bundle it into a separate Gradle module.

### 1.4 Remove the `gomobile` Gradle plugin

```groovy
// REMOVE from build.gradle
apply plugin: 'org.golang.mobile.bind'
gomobile { ... }
```

No equivalent plugin is needed for the Rust build.

---

## 2. Import Changes

```kotlin
// BEFORE (Go AAR — gomobile namespace)
import mobile.Yggstack
import mobile.LogCallback

// AFTER (Rust AAR — UniFFI namespace)
import uniffi.yggstack_mobile.YggstackMobile
import uniffi.yggstack_mobile.LogCallback
import uniffi.yggstack_mobile.YggstackError
import uniffi.yggstack_mobile.generateConfig   // free function
import uniffi.yggstack_mobile.getVersion       // free function
```

---

## 3. Class and Constructor

| Go | Rust |
|----|------|
| `Yggstack()` | `YggstackMobile()` |

```kotlin
// BEFORE
val ygg = Yggstack()

// AFTER
val ygg = YggstackMobile()
```

---

## 4. Config Format: JSON → TOML

The Go library accepted JSON (or HJSON). The Rust library uses **TOML**.

### 4.1 Generate a new config

```kotlin
// BEFORE — returns (String, Exception) tuple via gomobile
val (cfgJson, err) = GenerateConfig()

// AFTER — free function, returns TOML String directly (no error path)
val cfgToml: String = generateConfig()
```

### 4.2 Load a config string

```kotlin
// BEFORE
ygg.loadConfigJSON(cfgJson)

// AFTER — throws YggstackError on parse failure
try {
    ygg.loadConfig(cfgToml)
} catch (e: YggstackError.Config) {
    Log.e(TAG, "Bad config: ${e.message}")
}
```

### 4.3 Generate and load in one step

```kotlin
// BEFORE — no equivalent; had to call GenerateConfig() then loadConfigJSON()

// AFTER
ygg.generateAndLoadConfig()  // generates a fresh config and loads it atomically
```

### 4.4 Retrieve the current config

```kotlin
// BEFORE — no equivalent

// AFTER — returns TOML string, or empty string if nothing loaded
val toml: String = ygg.getConfig()
```

### 4.5 Key format in TOML config

In the Go JSON config the private key was stored as a hex string under `PrivateKey`.
In the Rust TOML config it is stored under `private_key` (snake_case).  You must
**convert any saved config files** before passing them to `loadConfig()`.

A minimal TOML config looks like:

```toml
private_key = "aabbcc..."   # 64-byte ed25519 private key, hex-encoded

[[peers]]
uri = "tcp://some.peer.example.com:17117"
```

---

## 5. Logging

Both libraries use the same callback interface name and method signature, but the
package is different (see §2).

```kotlin
// BEFORE
ygg.setLogCallback(object : mobile.LogCallback {
    override fun onLog(message: String) { Log.d(TAG, message) }
})

// AFTER
ygg.setLogCallback(object : LogCallback {
    override fun onLog(message: String) { Log.d(TAG, message) }
})
```

`setLogLevel(level: String)` accepts the same values in both libraries:
`"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`.

---

## 6. Start / Stop

### 6.1 `start()`

The Go library accepted `socksAddress` and `nameserver` as `start()` parameters.
The Rust library uses **separate setter methods** that must be called before `start()`.

```kotlin
// BEFORE
ygg.start("127.0.0.1:1080", "200:dead::1")

// AFTER
ygg.setSocks("127.0.0.1:1080")       // empty string or omit to disable SOCKS5
ygg.setNameserver("200:dead::1")     // empty string to disable
ygg.start()                          // throws YggstackError on failure
```

Wrap `start()` in a try/catch:

```kotlin
try {
    ygg.start()
} catch (e: YggstackError.AlreadyRunning) {
    // already running
} catch (e: YggstackError.Config) {
    // config not loaded or invalid
} catch (e: YggstackError) {
    Log.e(TAG, "Start failed: ${e.message}")
}
```

### 6.2 `stop()`

```kotlin
// BEFORE — returned an error
val err = ygg.stop()

// AFTER — returns Unit; silently no-ops if not running
ygg.stop()
```

### 6.3 `isRunning()`

Both libraries expose `isRunning()`.  The Rust method is named `isRunning()` and
returns `Boolean`:

```kotlin
// BEFORE
val running: Boolean = ygg.isRunning()

// AFTER
val running: Boolean = ygg.isRunning()
```

The method is safe to call from any thread at any time.

---

## 7. Node Identity

All three methods now throw `YggstackError` instead of returning a Go-style error tuple.

```kotlin
// BEFORE
val (addr, err) = ygg.getAddress()
val (subnet, err) = ygg.getSubnet()
val (pubkey, err) = ygg.getPublicKey()

// AFTER
val addr:   String = ygg.getAddress()    // throws YggstackError
val subnet: String = ygg.getSubnet()     // e.g. "300:aabb::/64"
val pubkey: String = ygg.getPublicKey()  // hex string
```

---

## 8. Port Mappings

The mapping API has been redesigned.  The Rust API uses **spec strings** rather than
separate address parameters, and all spec methods throw `YggstackError.Config` on
invalid input.

### 8.1 Local TCP forward  (local port → Yggdrasil host)

```kotlin
// BEFORE
ygg.addLocalTCPMapping("127.0.0.1:8080", "[200:1234::1]:80")

// AFTER — spec format: "<listen-port>:[<ygg-host>]:<remote-port>"
ygg.addLocalTcp("8080:[200:1234::1]:80")
```

### 8.2 Local UDP forward

```kotlin
// BEFORE
ygg.addLocalUDPMapping("127.0.0.1:5353", "[200:dead::1]:53")

// AFTER
ygg.addLocalUdp("5353:[200:dead::1]:53")
```

### 8.3 Remote TCP expose  (Yggdrasil port → local service)

The Go API took a raw port integer plus a local address string.
The Rust API takes a spec string: `"<remote-port>"` (binds to our own Yggdrasil
address) or `"<remote-port>:<local-port>"` when the local and remote ports differ.

```kotlin
// BEFORE
ygg.addRemoteTCPMapping(22, "127.0.0.1:22")   // expose local SSH
ygg.addRemoteTCPMapping(2222, "127.0.0.1:22") // expose on different port

// AFTER
ygg.addRemoteTcp("22")         // expose port 22 on our Ygg address → local :22
ygg.addRemoteTcp("2222:22")    // expose port 2222 → local :22
```

### 8.4 Remote UDP expose

```kotlin
// BEFORE
ygg.addRemoteUDPMapping(53, "127.0.0.1:53")

// AFTER
ygg.addRemoteUdp("53")
```

### 8.5 Clearing mappings

```kotlin
// BEFORE — two separate methods
ygg.clearLocalMappings()
ygg.clearRemoteMappings()

// AFTER — one method clears everything
ygg.clearMappings()
```

### 8.6 Removing individual mappings at runtime

The Rust library does **not** currently expose individual mapping removal
(`removeLocalTCPMapping`, `removeRemoteTCPMapping`, etc.).
To change mappings at runtime:

```kotlin
ygg.stop()
ygg.clearMappings()
// re-add desired mappings
ygg.addLocalTcp("8080:[200:1234::1]:80")
ygg.start()
```

### 8.7 Mappings added before vs after `start()`

Both libraries accept mappings before `start()`.
The **Go** library also supported adding mappings after `start()` (they were activated
immediately).  The **Rust** library requires all mappings to be configured before
`start()` — mappings added after start have no effect until the next restart.

---

## 9. Peer Management

The Go library offered live peer management methods that worked without restarting:

| Go method | Status in Rust |
|-----------|---------------|
| `addPeer(uri)` | **Config only** — add the URI to the TOML `[[peers]]` list then call `loadConfig()` before the next `start()` |
| `removePeer(uri)` | Same |
| `getPeers()` | Not available — read from the TOML config |
| `getPeersJSON()` | Not available |
| `addLivePeer(uri)` | ✅ `addLivePeer(uri)` — adds and connects immediately while running |
| `removeLivePeer(uri)` | ✅ `removeLivePeer(uri)` — disconnects and removes immediately while running |
| `retryPeersNow()` | ✅ `retryPeersNow()` — wakes all sleeping reconnect loops |

All three live-peer methods throw `YggstackError.NotRunning` if called before `start()`
and `YggstackError.Runtime` on a peer-level failure.

```kotlin
// Add a peer to a running node
try {
    ygg.addLivePeer("tcp://new.peer.example.com:17117")
} catch (e: YggstackError.NotRunning) {
    // call start() first
} catch (e: YggstackError.Runtime) {
    Log.e(TAG, "Peer error: ${e.message}")
}

// Remove a peer from a running node
ygg.removeLivePeer("tcp://old.peer.example.com:17117")

// Force immediate reconnect (e.g. after network change)
ygg.retryPeersNow()
```

**Pattern for switching all peers at runtime** (if you also need to update the stored
config so the new peers survive a restart):

```kotlin
ygg.retryPeersNow()           // if just waking up offline peers is enough
// — or —
ygg.removeLivePeer(oldUri)
ygg.addLivePeer(newUri)
// Update stored config for persistence:
val updated = ygg.getConfig().replace(...)
ygg.loadConfig(updated)       // takes effect on the next start()
```

---

## 10. Error Handling

The Go library used Go's multi-return convention which gomobile surfaced as a
checked `Exception`.  The Rust UniFFI library throws typed `YggstackError` variants:

| Variant | When thrown |
|---------|-------------|
| `YggstackError.Config(message)` | Invalid config, bad mapping spec, config not loaded |
| `YggstackError.Runtime(message)` | Internal runtime failure |
| `YggstackError.Io(message)` | I/O error (e.g. can't bind port) |
| `YggstackError.AlreadyRunning(message)` | `start()` called while running |
| `YggstackError.NotRunning(message)` | Reserved for future use |

Catch the sealed class base to handle all errors generically:

```kotlin
try {
    ygg.start()
} catch (e: YggstackError) {
    Log.e(TAG, "Yggstack error: ${e.message}")
}
```

---

## 11. Namespace (Free) Functions

```kotlin
// BEFORE — static-like via gomobile
val (cfg, err) = GenerateConfig()

// AFTER — Kotlin top-level functions in the uniffi.yggstack_mobile package
val cfg: String = generateConfig()   // returns fresh TOML config
val ver: String = getVersion()       // e.g. "yggstack 0.1.0"
```

---

## 12. Threading

The Go library ran its own goroutines internally and was safe to call from any thread.

The Rust library also manages its own Tokio async runtime internally; all public
methods are **blocking** (they block the calling thread until complete).  Call them
from a background thread or coroutine:

```kotlin
// Recommended: wrap in a coroutine dispatcher
lifecycleScope.launch(Dispatchers.IO) {
    ygg.start()
}
```

`stop()` is also blocking — it waits for the runtime to shut down.

---

## 13. Complete Migration Example

### Before (Kotlin + Go AAR)

```kotlin
import mobile.Yggstack
import mobile.GenerateConfig

val ygg = Yggstack()
ygg.setLogCallback(object : mobile.LogCallback {
    override fun onLog(msg: String) = Log.d("Ygg", msg)
})
ygg.setLogLevel("info")

val (cfg, _) = GenerateConfig()
ygg.loadConfigJSON(cfg)

ygg.addLocalTCPMapping("127.0.0.1:8080", "[200:ab::1]:80")
ygg.addRemoteTCPMapping(22, "127.0.0.1:22")

ygg.start("127.0.0.1:1080", "200:dead::1")

Log.i("Ygg", "Address: ${ygg.getAddress().component1()}")
```

### After (Kotlin + Rust AAR)

```kotlin
import uniffi.yggstack_mobile.*

val ygg = YggstackMobile()
ygg.setLogCallback(object : LogCallback {
    override fun onLog(msg: String) = Log.d("Ygg", msg)
})
ygg.setLogLevel("info")

ygg.generateAndLoadConfig()          // or: ygg.loadConfig(generateConfig())

ygg.addLocalTcp("8080:[200:ab::1]:80")
ygg.addRemoteTcp("22")

ygg.setSocks("127.0.0.1:1080")
ygg.setNameserver("200:dead::1")

try {
    ygg.start()
} catch (e: YggstackError) {
    Log.e("Ygg", "Failed: ${e.message}")
    return
}

Log.i("Ygg", "Address: ${ygg.getAddress()}")
```

---

## 14. Feature Parity Summary

| Feature | Go AAR | Rust AAR |
|---------|--------|----------|
| SOCKS5 proxy | ✅ | ✅ |
| DNS nameserver | ✅ | ✅ |
| Local TCP forward | ✅ | ✅ |
| Local UDP forward | ✅ | ✅ |
| Remote TCP expose | ✅ | ✅ |
| Remote UDP expose | ✅ | ✅ |
| Custom log callback | ✅ | ✅ |
| Config generation | ✅ JSON | ✅ TOML |
| Get address/subnet/pubkey | ✅ | ✅ |
| Live peer add/remove | ✅ | ✅ `addLivePeer()` / `removeLivePeer()` |
| Peer retry on network change | ✅ `retryPeersNow()` | ✅ `retryPeersNow()` |
| Per-mapping enable/disable | ✅ | ❌ restart required |
| `isRunning()` query | ✅ | ✅ |
| Multicast discovery | ✅ (config-driven) | ✅ (config-driven) |
| IPv6 fragment reassembly | ❌ | ✅ (custom `frag.rs`) |
