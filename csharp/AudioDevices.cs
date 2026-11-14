using NAudio.CoreAudioApi;

namespace MicBridge;

public static class AudioDevices
{
    public static MMDevice? FindCapture(string nameFilter)
    {
        var en = new MMDeviceEnumerator();
        if (string.IsNullOrWhiteSpace(nameFilter))
        {
            try { return en.GetDefaultAudioEndpoint(DataFlow.Capture, Role.Communications); }
            catch { return null; }
        }
        return en.EnumerateAudioEndPoints(DataFlow.Capture, DeviceState.Active)
                 .FirstOrDefault(d => d.FriendlyName.Contains(nameFilter, StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>
    /// Host side: the playback endpoint whose audio comes back out of a virtual microphone.
    /// Auto order: VB-Cable ("CABLE Input"), then Steam Streaming Microphone (its render endpoint is "Speakers (Steam Streaming Microphone)").
    /// </summary>
    public static MMDevice? FindVirtualMicOutput(string nameFilter, out string hint)
    {
        hint = "";
        var en = new MMDeviceEnumerator();
        var active = en.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Active).ToList();
        if (!string.IsNullOrWhiteSpace(nameFilter))
        {
            var d = active.FirstOrDefault(x => x.FriendlyName.Contains(nameFilter, StringComparison.OrdinalIgnoreCase));
            if (d == null) hint = $"No active playback device matches \"{nameFilter}\". Run: micbridge devices";
            return d;
        }
        foreach (var pref in new[] { "CABLE Input", "Steam Streaming Microphone" })
        {
            var d = active.FirstOrDefault(x => x.FriendlyName.Contains(pref, StringComparison.OrdinalIgnoreCase));
            if (d != null) return d;
        }
        var disabled = en.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Disabled | DeviceState.Unplugged)
                         .FirstOrDefault(x => x.FriendlyName.Contains("Steam Streaming Microphone", StringComparison.OrdinalIgnoreCase));
        hint = disabled != null
            ? "\"Speakers (Steam Streaming Microphone)\" exists but is disabled. Enable it: Win+R > mmsys.cpl > Playback tab > right-click > Show Disabled Devices > Enable. Do the same on the Recording tab."
            : "No virtual microphone found. Install VB-Cable (vb-audio.com/Cable) or enable the Steam Streaming Microphone device, then restart micbridge. Or set outputDevice in micbridge.json.";
        return null;
    }

    public static string Describe()
    {
        var en = new MMDeviceEnumerator();
        var sb = new System.Text.StringBuilder();
        foreach (var (flow, title) in new[] { (DataFlow.Capture, "Microphones (capture)"), (DataFlow.Render, "Playback (render)") })
        {
            sb.AppendLine(title + ":");
            foreach (var d in en.EnumerateAudioEndPoints(flow, DeviceState.All))
            {
                string state;
                try { state = d.State.ToString(); } catch { state = "?"; }
                string name;
                try { name = d.FriendlyName; } catch { name = d.ID; }
                sb.AppendLine($"  [{state,-9}] {name}");
            }
        }
        return sb.ToString();
    }
}
