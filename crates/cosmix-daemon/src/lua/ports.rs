//! ARexx-style port system for Lua.
//!
//! Exposes `cosmix.port("name")` which returns a `LuaPort` handle.
//! Calling any method on the handle will eventually route to a Unix socket
//! connection to the named app (Phase 3 / Layer 3). For now, method calls
//! return a helpful error indicating the port system is not yet active.

use mlua::prelude::*;

/// A handle to a named application port.
///
/// In the ARexx model, every COSMIC app registers a port with named commands.
/// Lua scripts obtain a port handle via `cosmix.port("name")` and then call
/// methods on it, e.g. `port:get()`, `port:search({ query = "foo" })`.
#[derive(Clone)]
struct LuaPort {
    name: String,
}

impl LuaUserData for LuaPort {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        // __index metamethod: intercept any method access on the port handle.
        // Returns a callable function that, when invoked, produces a clear
        // error explaining the port system is not yet connected.
        methods.add_meta_method(LuaMetaMethod::Index, |lua, this, key: String| {
            let port_name = this.name.clone();
            let method_name = key.clone();

            let func = lua.create_function(move |_, args: LuaMultiValue| {
                let _ = args; // acknowledge arguments, unused for now
                Err::<LuaValue, _>(LuaError::RuntimeError(format!(
                    "Port '{}' not connected (daemon port system not yet active). \
                     Method '{}' cannot be dispatched.\n\
                     \n\
                     The port API is a Phase 3 (Layer 3) feature. To use it, apps must \
                     integrate the cosmix-port crate and register their commands via \
                     Unix socket IPC at /run/user/$UID/cosmix/{}.sock.\n\
                     \n\
                     See: cosmix CLAUDE.md § \"The cosmix-port Crate\" and § \"Implementation Phases\"",
                    port_name, method_name, port_name,
                )))
            })?;

            Ok(LuaValue::Function(func))
        });

        // __tostring for convenient display in the REPL
        methods.add_meta_method(LuaMetaMethod::ToString, |_, this, ()| {
            Ok(format!("LuaPort(\"{}\")", this.name))
        });
    }
}

/// Register `cosmix.port(name)` on the given Lua instance.
///
/// Call this after the `cosmix` global table has been created.
pub fn register_port_api(lua: &Lua) -> LuaResult<()> {
    let cosmix: LuaTable = lua.globals().get("cosmix")?;

    cosmix.set(
        "port",
        lua.create_function(|_, name: String| {
            Ok(LuaPort { name })
        })?,
    )?;

    Ok(())
}
