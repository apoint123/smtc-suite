# GEMINI.md

This document provides a comprehensive overview of the `smtc-suite` project, designed to be used as a context file for AI assistants. It details the project's architecture, technologies, and development conventions to enable more accurate and context-aware assistance.

---

## 🚀 Project Overview

`smtc-suite` is a high-level Rust library for interacting with Windows' native media features. Its primary goals are to monitor and control media sessions via the System Media Transport Controls (SMTC), capture system audio output, and manage per-application volume. It is designed to be a robust backend for applications that need deep integration with the Windows media ecosystem.

### Key Technologies

* **Concurrency**: **Tokio** is used for the asynchronous runtime. The library specifically employs a **current-thread runtime** with a `LocalSet` to manage APIs that are not `Send` (i.e., must remain on their creation thread).
* **Windows Interoperability**: The **`windows-rs`** crate is used extensively for interacting with the Windows API. This includes:
    * **WinRT APIs**: For modern features like SMTC (`MediaSessionManager`).
    * **Win32 APIs**: For lower-level functionality like WASAPI audio capture (`IAudioClient`), COM initialization, and process enumeration.
* **Key Dependencies**:
    * **`rubato`**: For high-quality, configurable audio resampling.
    * **`ferrous-opencc`**: For Simplified/Traditional Chinese text conversion of media metadata.
    * **`log`**: For structured, level-based diagnostic logging.
    * **`easer`**: For smooth, animated volume transitions.

### Architecture

The library is built on a message-passing **actor model** to safely handle interactions with thread-sensitive Windows APIs.

1.  **Dedicated Worker Thread**: The entry point, `MediaManager::start()`, spawns a single, dedicated worker thread. This thread is crucial as it hosts the Tokio runtime and initializes a COM **Single-Threaded Apartment (STA)**, a requirement for many Windows UI and media components.

2.  **Message Passing Interface**: The client communicates with the worker thread through channels:
    * **`MediaController`**: A handle given to the user, containing a `tokio::mpsc::Sender` to send `MediaCommand` enums (e.g., `Play`, `Pause`, `SelectSession`) to the worker.
    * **`mpsc::Receiver<MediaUpdate>`**: A channel receiver through which the worker thread sends events and state changes (e.g., `TrackChanged`, `SessionsChanged`) back to the client.

3.  **Internal Coordinator**: The `MediaWorker` struct acts as the central hub on the worker thread. It listens for incoming `MediaCommand`s, translates them into `InternalCommand`s for its sub-modules, and aggregates `InternalUpdate`s from those modules into the public `MediaUpdate` events.

4.  **Specialized Sub-Modules**: The core logic is broken down into specialized modules that are managed by the worker:
    * **`smtc_handler`**: Manages all direct interaction with the Windows SMTC API.
    * **`audio_capture`**: Handles low-level audio loopback capture via WASAPI.
    * **`volume_control` / `audio_session_monitor`**: Manages per-application audio session enumeration and volume control.

5.  **Foreign Function Interface (FFI)**: A C-compatible FFI layer is defined in `ffi.rs`. This allows the Rust library to be compiled as a dynamic-link library (DLL) and used by other programming languages (C++, C#, Python, etc.), demonstrating its role as a versatile backend component.

---

## 🛠️ Development Conventions

The codebase follows several key conventions that ensure its robustness, safety, and maintainability.

### Error Handling

* A custom `SmtcError` enum, derived with `thiserror`, provides structured and descriptive errors for the library's public API.
* Functions that interact directly with the `windows-rs` crate typically return `windows::core::Result` (`WinResult`), which is then wrapped into the library's custom `Result` type.
* **FFI Safety**: Panics are explicitly caught at the FFI boundary using `std::panic::catch_unwind`. This is a critical convention to prevent Rust's panic unwinding from corrupting the stack of the C caller.

### Resource Management

* The project strictly adheres to the **RAII (Resource Acquisition Is Initialization)** pattern for managing all Windows resources.
* Custom "guard" structs (e.g., `ComGuard`, `AudioClientGuard`, `EventHandleGuard`) are used to ensure that COM objects are released, `HANDLE`s are closed, and allocated memory is freed automatically when they go out of scope, even in the event of an error.
* This pattern is extended to the FFI layer with guards like `StringGuard` and `NowPlayingInfoGuard` to safely manage the lifetime of data passed to C.

### Concurrency and Asynchronous Code

* The library's core is asynchronous but intentionally runs on a **single-threaded** Tokio runtime (`LocalSet`). This is a deliberate architectural choice to satisfy the STA threading requirements of the Windows APIs it consumes.
* Shared state within the single-threaded async context is managed using `Rc<RefCell<...>>` (as seen in `smtc_handler.rs`).
* Shared state that must be accessible across threads (e.g., between the client and worker) uses `Arc<Mutex<...>>`.
* Graceful shutdown of background tasks (like progress timers or volume animations) is handled using `tokio_util::sync::CancellationToken`.

### Code Style and Organization

* **Modularity**: The codebase is highly modular, with each file or module having a clear, single responsibility (e.g., `api.rs` for public types, `audio_capture.rs` for audio, `worker.rs` for coordination).
* **API Boundary**: A clear distinction is maintained between the public API (`api.rs`) and internal communication types (`worker.rs`).
* **`unsafe` Code**: The use of `unsafe` is necessary for FFI and direct Win32 interop. It is carefully encapsulated and accompanied by `Safety` doc comments explaining the invariants that must be upheld by the caller.
* **Logging**: The `log` crate is used throughout the library for diagnostics. The FFI layer provides a bridge to redirect these logs to a C callback, making it easy to debug when the library is used as a DLL.