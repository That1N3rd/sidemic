using System.Diagnostics;

namespace MicBridge;

/// <summary>System-tray front end. All the real work happens on background threads in MicClient / MicHost.</summary>
public sealed class TrayApp : ApplicationContext
{
    private readonly NotifyIcon _icon;
    private readonly ToolStripMenuItem _status;
    private readonly ToolStripMenuItem? _mute;
    private readonly Func<string> _getStatus;
    private readonly Action _onExit;
    private readonly System.Windows.Forms.Timer _timer;

    public TrayApp(string title, Func<string> getStatus, Action onExit, Action<bool>? setMuted, Func<bool>? isMuted, string? pairingKey, string configPath)
    {
        _getStatus = getStatus; _onExit = onExit;
        var menu = new ContextMenuStrip();
        _status = new ToolStripMenuItem(title) { Enabled = false };
        menu.Items.Add(_status);
        menu.Items.Add(new ToolStripSeparator());

        if (setMuted != null && isMuted != null)
        {
            _mute = new ToolStripMenuItem("Mute microphone") { CheckOnClick = true, Checked = isMuted() };
            _mute.CheckedChanged += (_, _) => setMuted(_mute.Checked);
            menu.Items.Add(_mute);
        }
        if (pairingKey != null)
        {
            menu.Items.Add("Copy pairing key", null, (_, _) =>
            {
                try { Clipboard.SetText(pairingKey); _icon!.ShowBalloonTip(2000, "MicBridge", "Pairing key copied. Paste it into the laptop's micbridge client --key ...", ToolTipIcon.Info); }
                catch (Exception ex) { MessageBox.Show(ex.Message, "MicBridge"); }
            });
        }
        menu.Items.Add("Open config file", null, (_, _) => Open(configPath));
        menu.Items.Add("Open log", null, (_, _) => { if (Log.FilePath != null) Open(Log.FilePath); else MessageBox.Show(Log.RecentText(), "MicBridge log"); });
        menu.Items.Add(new ToolStripSeparator());
        menu.Items.Add("Exit", null, (_, _) => Quit());
        menu.Opening += (_, _) => _status.Text = Shorten(_getStatus(), 100);

        _icon = new NotifyIcon
        {
            Icon = LoadIcon(),
            Text = Shorten("MicBridge - " + getStatus(), 120),
            Visible = true,
            ContextMenuStrip = menu,
        };
        _icon.DoubleClick += (_, _) => _icon.ShowBalloonTip(3000, "MicBridge", getStatus(), ToolTipIcon.Info);
        _icon.ShowBalloonTip(3000, "MicBridge " + title, getStatus(), ToolTipIcon.Info);

        _timer = new System.Windows.Forms.Timer { Interval = 1000 };
        _timer.Tick += (_, _) =>
        {
            _icon.Text = Shorten("MicBridge - " + _getStatus(), 120);
            if (_mute != null && isMuted != null && _mute.Checked != isMuted()) _mute.Checked = isMuted();
        };
        _timer.Start();
    }

    private static string Shorten(string s, int max) => s.Length <= max ? s : s[..(max - 1)] + "…";

    private static Icon LoadIcon()
    {
        try
        {
            var exe = Environment.ProcessPath;
            if (exe != null) { var ic = Icon.ExtractAssociatedIcon(exe); if (ic != null) return ic; }
        }
        catch { }
        return SystemIcons.Application;
    }

    private static void Open(string path)
    {
        try { Process.Start(new ProcessStartInfo(path) { UseShellExecute = true }); }
        catch (Exception ex) { MessageBox.Show($"{path}\n\n{ex.Message}", "MicBridge"); }
    }

    private void Quit()
    {
        _timer.Stop();
        _icon.Visible = false;
        try { _onExit(); } catch { }
        _icon.Dispose();
        ExitThread();
    }
}
