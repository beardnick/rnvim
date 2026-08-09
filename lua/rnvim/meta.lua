-- version: the plugin itself — bump freely, plugin managers track master.
-- agent_version: the rnvim-agent release deployed to remotes (binaries
-- are version-stamped there and pulled from the GitHub release with this
-- exact version). Bump ONLY when the Rust agent or protocol changes, and
-- tag that release — Lua-only changes never need a tag.
return { version = "0.10.3", agent_version = "0.10.2" }
