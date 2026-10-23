using System.Diagnostics;
using System.Globalization;

namespace ResourcesHelper.Tray.Spike;

/// <summary>
/// M0.2 spike only: a timestamped trace in %TEMP%\rh-tray-spike.log for the manual checklist.
/// </summary>
internal static class SpikeLog
{
    private static readonly string LogPath = Path.Combine(Path.GetTempPath(), "rh-tray-spike.log");

    public static void Write(string message)
    {
        var line = string.Create(
            CultureInfo.InvariantCulture,
            $"{DateTime.Now:HH:mm:ss.fff} [{Environment.TickCount64}] {message}");
        Debug.WriteLine(line);
        File.AppendAllText(LogPath, line + Environment.NewLine);
    }
}
