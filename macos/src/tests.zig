const std = @import("std");
const main = @import("main.zig");

const testing = std.testing;

test "serve argv keeps serve, static-dir, and db as separate arguments" {
    var argv: [main.serve_argv_len][]const u8 = undefined;
    const args = main.serveArgv(
        &argv,
        "/Apps/Mercator.app/Contents/Resources/assets/mercator",
        "/Apps/Mercator.app/Contents/Resources/assets/dist",
        "/Users/anon/Library/Application Support/Mercator/mercator.db",
        "/Users/anon/Library/Application Support/Mercator/mercator_map.json",
    );

    try testing.expectEqual(@as(usize, 12), args.len);
    try testing.expectEqualStrings("/Apps/Mercator.app/Contents/Resources/assets/mercator", args[0]);
    try testing.expectEqualStrings("serve", args[1]);
    try testing.expectEqualStrings("--static-dir", args[2]);
    try testing.expectEqualStrings("/Apps/Mercator.app/Contents/Resources/assets/dist", args[3]);
    try testing.expect(std.fs.path.isAbsolute(args[3]));
    try testing.expectEqualStrings("--db", args[4]);
    try testing.expectEqualStrings("/Users/anon/Library/Application Support/Mercator/mercator.db", args[5]);
    try testing.expectEqualStrings("--map-file", args[6]);
    try testing.expectEqualStrings("/Users/anon/Library/Application Support/Mercator/mercator_map.json", args[7]);
    try testing.expect(std.fs.path.isAbsolute(args[7]));
    try testing.expectEqualStrings("--bind", args[8]);
    try testing.expectEqualStrings("127.0.0.1", args[9]);
    try testing.expectEqualStrings("--port", args[10]);
    try testing.expectEqualStrings("3000", args[11]);
}

test "a relative static dir is not spawnable" {
    try testing.expect(main.bundleIsSpawnable(
        "/Apps/Mercator.app/Contents/Resources/assets/mercator",
        "/Apps/Mercator.app/Contents/Resources/assets/dist",
        "/Users/anon/Library/Application Support/Mercator/mercator.db",
        "/Users/anon/Library/Application Support/Mercator/mercator_map.json",
    ));
    try testing.expect(!main.bundleIsSpawnable(
        "/Apps/Mercator.app/Contents/Resources/assets/mercator",
        "dist",
        "/Users/anon/Library/Application Support/Mercator/mercator.db",
        "/Users/anon/Library/Application Support/Mercator/mercator_map.json",
    ));
    try testing.expect(!main.bundleIsSpawnable(
        "/Apps/Mercator.app/Contents/Resources/assets/mercator",
        "/Apps/Mercator.app/Contents/Resources/assets/dist",
        "/Users/anon/Library/Application Support/Mercator/mercator.db",
        "mercator_map.json",
    ));
}

test "packaged bundle paths sit under Contents/Resources/assets" {
    var binary: [256]u8 = undefined;
    var static_dir: [256]u8 = undefined;
    const bundle = (try main.packagedBundle(
        "/Apps/Mercator.app/Contents/MacOS/Mercator",
        &binary,
        &static_dir,
    )).?;
    try testing.expectEqualStrings("/Apps/Mercator.app/Contents/Resources/assets/mercator", bundle.binary);
    try testing.expectEqualStrings("/Apps/Mercator.app/Contents/Resources/assets/dist", bundle.static_dir);
    try testing.expect(std.fs.path.isAbsolute(bundle.static_dir));
}

test "dev bundle paths cover the project root and the macos directory" {
    var binary: [160]u8 = undefined;
    var static_dir: [160]u8 = undefined;
    const from_root = try main.devBundle("/repo", &binary, &static_dir, true);
    try testing.expectEqualStrings("/repo/macos/assets/mercator", from_root.binary);
    try testing.expectEqualStrings("/repo/macos/assets/dist", from_root.static_dir);

    const from_macos = try main.devBundle("/repo/macos", &binary, &static_dir, false);
    try testing.expectEqualStrings("/repo/macos/assets/mercator", from_macos.binary);
    try testing.expectEqualStrings("/repo/macos/assets/dist", from_macos.static_dir);
}

test "an executable outside a bundle has no packaged asset path" {
    var binary: [64]u8 = undefined;
    var static_dir: [64]u8 = undefined;
    const bundle = try main.packagedBundle("/tmp/mercator", &binary, &static_dir);
    try testing.expect(bundle == null);
}

test "the database and map file sit together under Application Support" {
    var buf: [160]u8 = undefined;
    const db_path = try main.applicationSupportDb("/Users/anon", &buf);
    try testing.expectEqualStrings("/Users/anon/Library/Application Support/Mercator/mercator.db", db_path);
    const map_path = try main.applicationSupportMap("/Users/anon", &buf);
    try testing.expectEqualStrings("/Users/anon/Library/Application Support/Mercator/mercator_map.json", map_path);
}

test "a stable server restarts once, and a cancel or a young exit does not" {
    const sdk = @import("native_sdk");

    const killed = main.afterChildExit(.ready, sdk.EffectExitReason.signaled, 1500, 1000, 0);
    try testing.expect(killed.respawn);
    try testing.expectEqual(main.Server.starting, killed.server);
    try testing.expectEqual(@as(u8, 1), killed.restarts_used);

    const panicked = main.afterChildExit(.ready, sdk.EffectExitReason.exited, 1600, 1000, 0);
    try testing.expect(panicked.respawn);

    const too_soon = main.afterChildExit(.ready, sdk.EffectExitReason.signaled, 1499, 1000, 0);
    try testing.expect(!too_soon.respawn);
    try testing.expectEqual(main.Server.exited, too_soon.server);

    const still_starting = main.afterChildExit(.starting, sdk.EffectExitReason.signaled, 5000, null, 0);
    try testing.expect(!still_starting.respawn);
    try testing.expectEqual(main.Server.exited, still_starting.server);

    const bind_panic = main.afterChildExit(.starting, sdk.EffectExitReason.exited, 5000, null, 0);
    try testing.expect(!bind_panic.respawn);

    const quit = main.afterChildExit(.ready, sdk.EffectExitReason.cancelled, 9000, 1000, 0);
    try testing.expect(!quit.respawn);
    try testing.expectEqual(main.Server.exited, quit.server);

    const second = main.afterChildExit(.ready, sdk.EffectExitReason.signaled, 9000, 1000, 1);
    try testing.expect(!second.respawn);
    try testing.expectEqual(@as(u8, 1), second.restarts_used);

    const unmarked = main.afterChildExit(.ready, sdk.EffectExitReason.signaled, 9000, null, 0);
    try testing.expect(!unmarked.respawn);
}

test "stdout is only a hint and a map 200 is what becomes ready" {
    const heard = main.noteStdout(.starting, "Mercator map available at http://127.0.0.1:3000");
    try testing.expectEqual(main.Server.starting, heard.server);
    try testing.expect(heard.announced);

    const other = main.noteStdout(.starting, "DB ready: 0 projects");
    try testing.expect(!other.announced);
    try testing.expectEqual(main.Server.starting, other.server);

    const after_exit = main.noteStdout(.exited, "Mercator map available at http://127.0.0.1:3000");
    try testing.expect(!after_exit.announced);

    try testing.expectEqual(main.Server.ready, main.nextServer(.starting, true, 200));
    try testing.expectEqual(main.Server.starting, main.nextServer(.starting, true, 503));
    try testing.expectEqual(main.Server.starting, main.nextServer(.starting, false, 200));
    try testing.expectEqual(main.Server.exited, main.nextServer(.exited, true, 200));
    try testing.expectEqual(main.Server.stopped, main.nextServer(.stopped, true, 200));
}
