/// Implementations of `IntoLua` / `FromLua` for Vector event types
/// (`Event`, `Metric`). `OtelLog` exposes its underlying `Value` tree for
/// Lua interop via `event::LuaEvent`.
pub mod event;
pub mod metric;
pub mod util;
