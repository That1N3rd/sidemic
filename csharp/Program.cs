using System.Diagnostics;
using System.Runtime.InteropServices;

namespace MicBridge;

internal static class Program
{
    private const string Usage = """
        MicBridge - send your laptop's microphone to the PC you stream from (Sunshine/Apollo + Moonlight).
        Independent of Moonlight and Sunshine, so their updates never break it.

        On the streamed PC (host):      micbridge host [--port 48020] [--output "CABLE Input"]
        On the laptop (client):         micbridge client --host <host-ip> --key <key-from-host> [--mic "name"]
        List audio devices:             micbridge devices
        Allow UDP port in firewall:     micbridge firewall        (host only, asks for admin)
        Start with Windows:             micbridge autostart       (micbridge autostart --remove to undo)

        Options:
          --port N          UDP port (default 48020, same on both ends)
          --key K           shared key; host generates one on first run (tray menu > Copy pairing key)
          --new-key         host: generate a fresh key
          --mic NAME        client: part of the microphone's name (default: Windows default comms mic)
          --output NAME     host: part of the playback device name that feeds the virtual mic
          --bitrate N       Opus bits/s (default 40000)
          --jitter MS       host: jitter cushion (default 60)
          --follow / --no-follow   client: only send while Moonlight.exe runs (default: follow)
          --console         run in the terminal instead of the tray
          --config PATH     use a different micbridge.json

        Settings are saved to micbridge.json, so after the first run you can just double-click micbridge.exe.
        """;

    [STAThread]
    private static int Main(string[] args)
    {
        var (verb, opts) = Parse(args);
        var console = opts.ContainsKey("console") || verb is "devices" or "help";
        if (console) EnableConsole();

        if (verb == "help" || opts.ContainsKey("help") || opts.ContainsKey("h") || opts.ContainsKey("?"))
        { Show(Usage); return 0; }

        var cfgPath = opts.TryGetValue("config", out var cp) ? cp : Config.DefaultPath();
        Log.Open(Path.Combine(Path.GetDirectoryName(cfgPath)!, "micbridge.log"));
        Log.ConsoleEnabled = console;

        if (verb == "devices") { Show(AudioDevices.Describe()); return 0; }

        Config cfg;
        try { cfg = Config.Load(cfgPath); }
        catch (Exception ex) { Show($"Could not read {cfgPath}:\n{ex.Message}"); return 2; }

        if (verb is "host" or "client") cfg.Mode = verb;
        if (opts.TryGetValue("host", out var h) && verb == "client") cfg.Host = h;
        if (opts.TryGetValue("port", out var p) && int.TryParse(p, out var port)) cfg.Port = port;
        if (opts.TryGetValue("key", out var k)) cfg.Key = k;
        if (opts.TryGetValue("mic", out var mic)) cfg.MicDevice = mic;
        if (opts.TryGetValue("output", out var outp)) cfg.OutputDevice = outp;
        if (opts.TryGetValue("bitrate", out var br) && int.TryParse(br, out var bitrate)) cfg.Bitrate = bitrate;
        if (opts.TryGetValue("jitter", out var jt) && int.TryParse(jt, out var jitter)) cfg.JitterMs = jitter;
        if (opts.ContainsKey("follow")) cfg.FollowMoonlight = true;
        if (opts.ContainsKey("no-follow")) cfg.FollowMoonlight = false;

        if (verb == "firewall") return AddFirewallRule(cfg.Port);
        if (verb == "autostart") return Autostart(remove: opts.ContainsKey("remove"));

        if (cfg.Mode is not ("host" or "client"))
        {
            Show("Not set up yet.\n\n" + Usage);
            return 2;
        }

        if (cfg.Mode == "host" && (string.IsNullOrWhiteSpace(cfg.Key) || opts.ContainsKey("new-key")))
        {
            cfg.Key = Config.NewKey();
            Log.Info("Generated a new pairing key");
        }
        try { cfg.Save(); } catch (Exception ex) { Log.Warn($"Could not save config to {cfgPath}: {ex.Message}"); }

        using var single = new Mutex(true, "Local\\MicBridge." + cfg.Mode, out var first);
        if (!first && !console) { Show($"MicBridge ({cfg.Mode}) is already running - look for it in the system tray."); return 1; }

        IDisposable service; Func<string> status;
        Action<bool>? setMuted = null; Func<bool>? isMuted = null;
        try
        {
            if (cfg.Mode == "host")
            {
                var hostSvc = new MicHost(cfg); hostSvc.Start();
                service = hostSvc; status = () => hostSvc.Status;
            }
            else
            {
                var client = new MicClient(cfg); client.Start();
                service = client; status = () => client.Status;
                setMuted = m => client.Muted = m; isMuted = () => client.Muted;
            }
        }
        catch (Exception ex)
        {
            Log.Error(ex.Message);
            Show($"MicBridge could not start:\n\n{ex.Message}\n\nLog: {Log.FilePath}");
            return 1;
        }

        if (cfg.Mode == "host") Log.Info($"Pairing key: {cfg.Key}");

        if (console)
        {
            Console.WriteLine(status());
            if (cfg.Mode == "host") Console.WriteLine($"Pairing key (give this to the laptop): {cfg.Key}");
            Console.WriteLine("Press Ctrl+C to stop.");
            using var done = new ManualResetEventSlim();
            Console.CancelKeyPress += (_, e) => { e.Cancel = true; done.Set(); };
            var last = "";
            while (!done.Wait(1000))
            {
                var s = status();
                if (s != last) { Console.WriteLine(s); last = s; }
            }
            service.Dispose();
            return 0;
        }

        Application.EnableVisualStyles();
        Application.SetHighDpiMode(HighDpiMode.PerMonitorV2);
        Application.Run(new TrayApp(cfg.Mode == "host" ? "Host (receiving mic)" : "Client (sending mic)",
            status, () => service.Dispose(), setMuted, isMuted, cfg.Mode == "host" ? cfg.Key : null, cfgPath));
        return 0;
    }

    private static (string verb, Dictionary<string, string> opts) Parse(string[] args)
    {
        var verb = ""; var opts = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        for (var i = 0; i < args.Length; i++)
        {
            var a = args[i];
            if (a.StartsWith("--") || a.StartsWith('-') || a.StartsWith('/'))
            {
                var name = a.TrimStart('-', '/');
                string val = "";
                var eq = name.IndexOf('=');
                if (eq >= 0) { val = name[(eq + 1)..]; name = name[..eq]; }
                else if (i + 1 < args.Length && !args[i + 1].StartsWith('-') && name is "host" or "port" or "key" or "mic" or "output" or "bitrate" or "jitter" or "config")
                    val = args[++i];
                opts[name] = val;
            }
            else if (verb == "") verb = a.ToLowerInvariant();
        }
        return (verb, opts);
    }

    private static int AddFirewallRule(int port)
    {
        var exe = Environment.ProcessPath ?? "micbridge.exe";
        var rule = $"advfirewall firewall add rule name=\"MicBridge (UDP {port})\" dir=in action=allow protocol=UDP localport={port} program=\"{exe}\"";
        try
        {
            var p = Process.Start(new ProcessStartInfo("netsh", rule) { UseShellExecute = true, Verb = "runas" });
            p?.WaitForExit();
            Show(p?.ExitCode == 0 ? $"Firewall rule added for UDP {port}." : $"netsh exited with code {p?.ExitCode}.");
            return p?.ExitCode ?? 1;
        }
        catch (Exception ex) { Show("Could not add firewall rule: " + ex.Message); return 1; }
    }

    /// <summary>Creates (or removes) a shortcut in the user's Startup folder so MicBridge runs at sign-in with its saved config.</summary>
    private static int Autostart(bool remove)
    {
        var lnk = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Startup), "MicBridge.lnk");
        try
        {
            if (remove)
            {
                if (File.Exists(lnk)) File.Delete(lnk);
                Show("MicBridge will no longer start with Windows.");
                return 0;
            }
            var exe = Environment.ProcessPath ?? throw new InvalidOperationException("Cannot determine exe path");
            var shellType = Type.GetTypeFromProgID("WScript.Shell") ?? throw new InvalidOperationException("WScript.Shell unavailable");
            dynamic shell = Activator.CreateInstance(shellType)!;
            dynamic sc = shell.CreateShortcut(lnk);
            sc.TargetPath = exe;
            sc.Arguments = "";
            sc.WorkingDirectory = Path.GetDirectoryName(exe);
            sc.IconLocation = exe + ",0";
            sc.Description = "MicBridge - microphone passthrough for game streaming";
            sc.Save();
            Show($"MicBridge will start with Windows using the settings in micbridge.json.\n\nShortcut: {lnk}");
            return 0;
        }
        catch (Exception ex) { Show("Autostart failed: " + ex.Message); return 1; }
    }

    // ---- console plumbing for a WinExe ----
    [DllImport("kernel32.dll")] private static extern bool AttachConsole(int pid);
    [DllImport("kernel32.dll")] private static extern bool AllocConsole();
    private static bool _haveConsole;

    private static void EnableConsole()
    {
        // stdout already points at a file/pipe (e.g. "micbridge host --console > out.txt"): nothing to attach.
        if (Console.IsOutputRedirected) { _haveConsole = true; return; }
        _haveConsole = AttachConsole(-1) || AllocConsole();
        if (!_haveConsole) return;
        Console.SetOut(new StreamWriter(Console.OpenStandardOutput()) { AutoFlush = true });
        Console.SetError(new StreamWriter(Console.OpenStandardError()) { AutoFlush = true });
        Console.WriteLine();
    }

    private static void Show(string text)
    {
        if (_haveConsole) Console.WriteLine(text);
        else MessageBox.Show(text, "MicBridge", MessageBoxButtons.OK, MessageBoxIcon.Information);
    }
}
