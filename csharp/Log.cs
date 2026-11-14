using System.Text;

namespace MicBridge;

/// <summary>Tiny thread-safe logger: writes to the console (when attached) and to micbridge.log.</summary>
public static class Log
{
    private static readonly object Gate = new();
    private static StreamWriter? _file;
    public static bool ConsoleEnabled;
    public static string? FilePath { get; private set; }

    /// <summary>Last ~200 lines kept in memory so the tray "Show log" can display them.</summary>
    private static readonly Queue<string> Recent = new();

    public static void Open(string path)
    {
        try
        {
            FilePath = path;
            // Keep the log from growing forever.
            if (File.Exists(path) && new FileInfo(path).Length > 2_000_000) File.Delete(path);
            _file = new StreamWriter(path, append: true, Encoding.UTF8) { AutoFlush = true };
        }
        catch { _file = null; }
    }

    public static void Info(string msg) => Write("INFO", msg);
    public static void Warn(string msg) => Write("WARN", msg);
    public static void Error(string msg) => Write("ERR ", msg);

    private static void Write(string level, string msg)
    {
        var line = $"{DateTime.Now:HH:mm:ss.fff} {level} {msg}";
        lock (Gate)
        {
            if (ConsoleEnabled) Console.WriteLine(line);
            _file?.WriteLine(line);
            Recent.Enqueue(line);
            while (Recent.Count > 200) Recent.Dequeue();
        }
    }

    public static string RecentText()
    {
        lock (Gate) return string.Join(Environment.NewLine, Recent);
    }
}
