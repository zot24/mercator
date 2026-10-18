//! Spawns the bundled Mercator binary and shows its dashboard.

const std = @import("std");
const runner = @import("runner");
const native_sdk = @import("native_sdk");

pub const panic = std.debug.FullPanic(native_sdk.debug.capturePanic);

const canvas = native_sdk.canvas;
const geometry = native_sdk.geometry;

const canvas_label = "main-canvas";
const webview_label = "dashboard";
const pane_anchor = "dashboard-pane";
const window_width: f32 = 1200;
const window_height: f32 = 800;

const spawn_key: u64 = 1;
const poll_key: u64 = 2;
const timer_key: u64 = 3;
const poll_ms: u32 = 200;
// The listen line is printed before the socket bind.
const settle_ms: i64 = 300;
const stable_ready_ms: i64 = 500;
const listen_hint = "Mercator map available";

pub const dashboard_origin = "http://127.0.0.1:3000";
pub const dashboard_url = "http://127.0.0.1:3000/";
pub const map_url = "http://127.0.0.1:3000/api/map";
pub const serve_argv_len = 12;

const blank_url = "zero://inline";
const allowed_origins = [_][]const u8{
    "zero://app",
    "zero://inline",
    dashboard_origin,
};
const app_permissions = [_][]const u8{
    native_sdk.security.permission_command,
    native_sdk.security.permission_view,
};

const shell_views = [_]native_sdk.ShellView{
    .{
        .label = canvas_label,
        .kind = .gpu_surface,
        .fill = true,
        .role = "Mercator",
        .accessibility_label = "Mercator",
        .gpu_backend = .metal,
        .gpu_pixel_format = .bgra8_unorm,
        .gpu_present_mode = .timer,
        .gpu_alpha_mode = .@"opaque",
        .gpu_color_space = .srgb,
        .gpu_vsync = true,
    },
    .{
        .label = webview_label,
        .kind = .webview,
        .parent = canvas_label,
        .url = blank_url,
        .x = 0,
        .y = 0,
        .width = 1,
        .height = 1,
    },
};
const shell_windows = [_]native_sdk.ShellWindow{.{
    .label = "main",
    .title = "Mercator",
    .width = window_width,
    .height = window_height,
    .restore_state = false,
    .views = &shell_views,
}};
const shell_scene: native_sdk.ShellConfig = .{ .windows = &shell_windows };

pub const Server = enum {
    stopped,
    starting,
    ready,
    exited,
};

pub const Msg = union(enum) {
    pub const view_unbound = .{ "line", "spawned", "polled", "tick" };

    line: native_sdk.EffectLine,
    spawned: native_sdk.EffectExit,
    polled: native_sdk.EffectResponse,
    tick: native_sdk.EffectTimer,
};

pub const Model = struct {
    pub const view_unbound = .{
        "server",
        "binary",
        "binary_len",
        "static_dir",
        "static_dir_len",
        "db_path",
        "db_path_len",
        "detail",
        "detail_len",
        "announced",
        "announced_ms",
        "poll_in_flight",
        "reload_token",
        "map_path",
        "map_path_len",
        "ready_ms",
        "restarts_used",
    };

    server: Server = .stopped,
    binary: [std.fs.max_path_bytes]u8 = undefined,
    binary_len: usize = 0,
    static_dir: [std.fs.max_path_bytes]u8 = undefined,
    static_dir_len: usize = 0,
    db_path: [std.fs.max_path_bytes]u8 = undefined,
    db_path_len: usize = 0,
    map_path: [std.fs.max_path_bytes]u8 = undefined,
    map_path_len: usize = 0,
    detail: [1024]u8 = undefined,
    detail_len: usize = 0,
    announced: bool = false,
    announced_ms: i64 = 0,
    poll_in_flight: bool = false,
    reload_token: u64 = 0,
    ready_ms: ?i64 = null,
    restarts_used: u8 = 0,

    pub fn showStatus(self: *const Model) bool {
        return self.server != .ready;
    }

    pub fn statusTitle(self: *const Model) []const u8 {
        return switch (self.server) {
            .stopped, .starting => "Starting Mercator",
            .ready => "Mercator",
            .exited => "Mercator did not start",
        };
    }

    pub fn statusDetail(self: *const Model) []const u8 {
        return self.detail[0..self.detail_len];
    }
};

pub const Bundle = struct {
    binary: []const u8,
    static_dir: []const u8,
};

const App = native_sdk.UiApp(Model, Msg);
const Effects = App.Effects;

pub const AppUi = canvas.Ui(Msg);
pub const app_markup = @embedFile("app.native");

pub fn serveArgv(
    out: *[serve_argv_len][]const u8,
    binary: []const u8,
    static_dir: []const u8,
    db_path: []const u8,
    map_path: []const u8,
) []const []const u8 {
    out.* = .{
        binary,
        "serve",
        "--static-dir",
        static_dir,
        "--db",
        db_path,
        "--map-file",
        map_path,
        "--bind",
        "127.0.0.1",
        "--port",
        "3000",
    };
    return out;
}

pub fn bundleIsSpawnable(binary: []const u8, static_dir: []const u8, db_path: []const u8, map_path: []const u8) bool {
    return binary.len > 0 and
        std.fs.path.isAbsolute(binary) and
        std.fs.path.isAbsolute(static_dir) and
        std.fs.path.isAbsolute(db_path) and
        std.fs.path.isAbsolute(map_path);
}

pub fn applicationSupportDb(home: []const u8, buf: []u8) error{NoSpaceLeft}![]u8 {
    return applicationSupportChild(home, "mercator.db", buf);
}

pub fn applicationSupportMap(home: []const u8, buf: []u8) error{NoSpaceLeft}![]u8 {
    return applicationSupportChild(home, "mercator_map.json", buf);
}

fn applicationSupportChild(home: []const u8, name: []const u8, buf: []u8) error{NoSpaceLeft}![]u8 {
    return std.fmt.bufPrint(buf, "{s}/Library/Application Support/Mercator/{s}", .{ home, name });
}

pub const ExitDecision = struct {
    server: Server,
    respawn: bool,
    restarts_used: u8,
};

pub fn afterChildExit(
    server: Server,
    reason: native_sdk.EffectExitReason,
    now_ms: i64,
    ready_ms: ?i64,
    restarts_used: u8,
) ExitDecision {
    switch (reason) {
        .cancelled, .rejected, .spawn_failed => return .{
            .server = .exited,
            .respawn = false,
            .restarts_used = restarts_used,
        },
        .signaled, .exited => {},
    }
    const stable = if (ready_ms) |marked| now_ms >= marked + stable_ready_ms else false;
    if (server == .ready and stable and restarts_used == 0) {
        return .{ .server = .starting, .respawn = true, .restarts_used = 1 };
    }
    return .{ .server = .exited, .respawn = false, .restarts_used = restarts_used };
}

pub fn packagedBundle(exe: []const u8, binary_buf: []u8, static_buf: []u8) error{NoSpaceLeft}!?Bundle {
    const marker = "/Contents/MacOS/";
    const idx = std.mem.indexOf(u8, exe, marker) orelse return null;
    const root = exe[0..idx];
    return .{
        .binary = try std.fmt.bufPrint(binary_buf, "{s}/Contents/Resources/assets/mercator", .{root}),
        .static_dir = try std.fmt.bufPrint(static_buf, "{s}/Contents/Resources/assets/dist", .{root}),
    };
}

pub fn devBundle(dir: []const u8, binary_buf: []u8, static_buf: []u8, at_repo_root: bool) error{NoSpaceLeft}!Bundle {
    if (at_repo_root) {
        return .{
            .binary = try std.fmt.bufPrint(binary_buf, "{s}/macos/assets/mercator", .{dir}),
            .static_dir = try std.fmt.bufPrint(static_buf, "{s}/macos/assets/dist", .{dir}),
        };
    }
    return .{
        .binary = try std.fmt.bufPrint(binary_buf, "{s}/assets/mercator", .{dir}),
        .static_dir = try std.fmt.bufPrint(static_buf, "{s}/assets/dist", .{dir}),
    };
}

pub fn noteStdout(server: Server, line: []const u8) struct { server: Server, announced: bool } {
    const announced = server == .starting and std.mem.indexOf(u8, line, listen_hint) != null;
    return .{ .server = server, .announced = announced };
}

pub fn nextServer(current: Server, outcome_ok: bool, map_status: u16) Server {
    if (current == .starting and outcome_ok and map_status == 200) return .ready;
    return current;
}

pub fn update(model: *Model, msg: Msg, fx: *Effects) void {
    switch (msg) {
        .line => |line| {
            const heard = noteStdout(model.server, line.line);
            if (!heard.announced) return;
            model.announced = true;
            model.announced_ms = fx.wallMs();
            setDetail(model, "Mercator announced the port. Checking /api/map.");
        },
        .tick => |timer| {
            if (timer.outcome != .fired) return;
            if (model.server != .starting) {
                fx.cancelTimer(timer_key);
                return;
            }
            if (!model.announced) return;
            if (fx.wallMs() - model.announced_ms < settle_ms) return;
            pollMap(model, fx);
        },
        .polled => |response| {
            model.poll_in_flight = false;
            const next = nextServer(model.server, response.outcome == .ok, response.status);
            if (next != .ready) return;
            model.server = .ready;
            model.ready_ms = fx.wallMs();
            model.reload_token += 1;
            fx.cancelTimer(timer_key);
            fx.cancel(poll_key);
        },
        .spawned => |exit| {
            if (model.server == .stopped) return;
            const decision = afterChildExit(
                model.server,
                exit.reason,
                fx.wallMs(),
                model.ready_ms,
                model.restarts_used,
            );
            model.restarts_used = decision.restarts_used;
            fx.cancel(poll_key);
            model.poll_in_flight = false;
            if (decision.respawn) {
                startChild(model, fx);
                return;
            }
            noteExit(model, exit);
            fx.cancelTimer(timer_key);
        },
    }
}

pub fn boot(model: *Model, fx: *Effects) void {
    if (model.server == .exited) return;
    if (!bundleIsSpawnable(binarySlice(model), staticSlice(model), dbSlice(model), mapSlice(model))) {
        fail(model, "The Mercator bundle is missing. Run macos/scripts/stage-assets.sh, then start the app again.");
        return;
    }
    startChild(model, fx);
}

fn startChild(model: *Model, fx: *Effects) void {
    model.server = .starting;
    model.announced = false;
    model.announced_ms = 0;
    model.ready_ms = null;
    model.poll_in_flight = false;
    setDetail(model, "Waiting for mercator serve.");
    var storage: [serve_argv_len][]const u8 = undefined;
    const argv = serveArgv(
        &storage,
        binarySlice(model),
        staticSlice(model),
        dbSlice(model),
        mapSlice(model),
    );
    fx.spawn(.{
        .key = spawn_key,
        .argv = argv,
        .on_line = Effects.lineMsg(.line),
        .on_exit = Effects.exitMsg(.spawned),
    });
    fx.startTimer(.{
        .key = timer_key,
        .interval_ms = poll_ms,
        .mode = .repeating,
        .on_fire = Effects.timerMsg(.tick),
    });
}

fn pollMap(model: *Model, fx: *Effects) void {
    if (model.poll_in_flight or model.server != .starting) return;
    model.poll_in_flight = true;
    fx.fetch(.{
        .key = poll_key,
        .url = map_url,
        .timeout_ms = 1000,
        .on_response = Effects.responseMsg(.polled),
    });
}

fn panes(model: *const Model, out: []App.WebViewPane) usize {
    if (model.server != .ready) {
        out[0] = .{
            .label = webview_label,
            .url = blank_url,
            .frame = geometry.RectF.init(0, 0, 1, 1),
        };
        return 1;
    }
    out[0] = .{
        .label = webview_label,
        .anchor = pane_anchor,
        .url = dashboard_url,
        .reload_token = model.reload_token,
    };
    return 1;
}

fn noteExit(model: *Model, exit: native_sdk.EffectExit) void {
    model.server = .exited;
    model.poll_in_flight = false;
    const written = switch (exit.reason) {
        .spawn_failed => std.fmt.bufPrint(&model.detail, "Could not start {s}.", .{binarySlice(model)}) catch return,
        .rejected => std.fmt.bufPrint(&model.detail, "Could not start Mercator. The command was rejected.", .{}) catch return,
        .signaled, .cancelled => std.fmt.bufPrint(&model.detail, "Mercator stopped.", .{}) catch return,
        .exited => std.fmt.bufPrint(&model.detail, "Mercator exited with code {d}.", .{exit.code}) catch return,
    };
    model.detail_len = written.len;
}

fn fail(model: *Model, text: []const u8) void {
    model.server = .exited;
    setDetail(model, text);
}

fn setDetail(model: *Model, text: []const u8) void {
    const n = @min(text.len, model.detail.len);
    @memcpy(model.detail[0..n], text[0..n]);
    model.detail_len = n;
}

fn binarySlice(model: *const Model) []const u8 {
    return model.binary[0..model.binary_len];
}

fn staticSlice(model: *const Model) []const u8 {
    return model.static_dir[0..model.static_dir_len];
}

fn dbSlice(model: *const Model) []const u8 {
    return model.db_path[0..model.db_path_len];
}

fn mapSlice(model: *const Model) []const u8 {
    return model.map_path[0..model.map_path_len];
}

fn prepare(model: *Model, init: std.process.Init) void {
    const home = init.environ_map.get("HOME") orelse {
        fail(model, "HOME is unset, so the database directory cannot be created.");
        return;
    };
    const db_path = applicationSupportDb(home, &model.db_path) catch {
        fail(model, "The database path does not fit.");
        return;
    };
    model.db_path_len = db_path.len;
    const map_path = applicationSupportMap(home, &model.map_path) catch {
        fail(model, "The map path does not fit.");
        return;
    };
    model.map_path_len = map_path.len;
    const support = std.fs.path.dirname(db_path) orelse {
        fail(model, "The database path has no directory.");
        return;
    };
    std.Io.Dir.cwd().createDirPath(init.io, support) catch {
        fail(model, "Could not create ~/Library/Application Support/Mercator.");
        return;
    };

    var exe_buf: [std.fs.max_path_bytes]u8 = undefined;
    const exe_len = std.process.executablePath(init.io, &exe_buf) catch {
        fail(model, "Could not read the app executable path.");
        return;
    };
    var cwd_buf: [std.fs.max_path_bytes]u8 = undefined;
    const cwd_len = std.process.currentPath(init.io, &cwd_buf) catch {
        fail(model, "Could not read the working directory.");
        return;
    };
    const cwd = cwd_buf[0..cwd_len];
    const located = locateBundle(init.io, exe_buf[0..exe_len], cwd, &model.binary, &model.static_dir) orelse {
        fail(model, "Could not find macos/assets/mercator or Contents/Resources/assets/mercator. Run macos/scripts/stage-assets.sh.");
        return;
    };
    model.binary_len = located.binary.len;
    model.static_dir_len = located.static_dir.len;
    if (!bundleIsSpawnable(binarySlice(model), staticSlice(model), dbSlice(model), mapSlice(model))) {
        fail(model, "The bundled dashboard path is not absolute.");
    }
}

fn locateBundle(
    io: std.Io,
    exe: []const u8,
    cwd: []const u8,
    binary_buf: []u8,
    static_buf: []u8,
) ?Bundle {
    if (packagedBundle(exe, binary_buf, static_buf) catch null) |packaged| {
        if (bundleFilesExist(io, packaged)) return packaged;
    }
    const exe_dir = std.fs.path.dirname(exe) orelse exe;
    if (walkBundle(io, exe_dir, binary_buf, static_buf)) |found| return found;
    if (walkBundle(io, cwd, binary_buf, static_buf)) |found| return found;
    return null;
}

fn walkBundle(io: std.Io, start: []const u8, binary_buf: []u8, static_buf: []u8) ?Bundle {
    var dir = start;
    var level: usize = 0;
    while (level < 12) : (level += 1) {
        if (devBundle(dir, binary_buf, static_buf, false) catch null) |local| {
            if (bundleFilesExist(io, local)) return local;
        }
        if (devBundle(dir, binary_buf, static_buf, true) catch null) |nested| {
            if (bundleFilesExist(io, nested)) return nested;
        }
        dir = std.fs.path.dirname(dir) orelse return null;
    }
    return null;
}

fn bundleFilesExist(io: std.Io, bundle: Bundle) bool {
    if (!isFile(io, bundle.binary)) return false;
    var index_buf: [std.fs.max_path_bytes]u8 = undefined;
    const index_path = std.fmt.bufPrint(&index_buf, "{s}/index.html", .{bundle.static_dir}) catch return false;
    return isFile(io, index_path);
}

fn isFile(io: std.Io, path: []const u8) bool {
    const stat = std.Io.Dir.cwd().statFile(io, path, .{}) catch return false;
    return stat.kind == .file;
}

pub fn main(init: std.process.Init) !void {
    const app_state = try App.create(std.heap.page_allocator, .{
        .name = "mercator",
        .scene = shell_scene,
        .canvas_label = canvas_label,
        .update_fx = update,
        .init_fx = boot,
        .web_panes = panes,
        .markup = .{ .source = app_markup, .watch_path = "src/app.native", .io = init.io },
    });
    defer app_state.destroy();
    prepare(&app_state.model, init);

    try runner.runWithOptions(app_state.app(), .{
        .app_name = "mercator",
        .window_title = "Mercator",
        .bundle_id = "dev.mercator.desktop",
        .icon_path = "assets/icon.png",
        .default_frame = geometry.RectF.init(0, 0, window_width, window_height),
        .restore_state = false,
        .js_window_api = false,
        .security = .{
            .permissions = &app_permissions,
            .navigation = .{ .allowed_origins = &allowed_origins },
        },
    }, init);
}

test {
    _ = @import("tests.zig");
}
