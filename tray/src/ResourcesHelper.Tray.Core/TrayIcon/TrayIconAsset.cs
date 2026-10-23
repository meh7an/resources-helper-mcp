using System.Globalization;

namespace ResourcesHelper.Tray.Core.TrayIcon;

/// <summary>
/// The four meter-mark states from the handoff (section 7).
/// </summary>
public enum TrayIconState
{
    Idle,
    Busy,
    Queued,
    Down,
}

/// <summary>
/// The taskbar's theme, which can differ from the app theme; the icon follows the taskbar.
/// </summary>
public enum TaskbarTheme
{
    Light,
    Dark,
}

/// <summary>
/// Which handoff ICO file and tooltip the tray icon shows.
/// </summary>
public static class TrayIconAsset
{
    /// <summary>
    /// NOTIFYICONDATAW.szTip holds 128 characters including the terminator.
    /// </summary>
    public const int MaxTooltipLength = 127;

    private const string AppName = "Resources Helper";

    public static string FileName(TrayIconState state, TaskbarTheme theme) =>
        string.Create(
            CultureInfo.InvariantCulture,
            $"rh-{StateName(state)}-on-{ThemeName(theme)}-taskbar.ico");

    /// <summary>
    /// Reads HKCU\...\Themes\Personalize\SystemUsesLightTheme. The value is missing only on
    /// Windows builds whose taskbar is always dark.
    /// </summary>
    public static TaskbarTheme ThemeFrom(int? systemUsesLightTheme) =>
        systemUsesLightTheme is null or 0 ? TaskbarTheme.Dark : TaskbarTheme.Light;

    public static string Tooltip(string summary)
    {
        var text = $"{AppName} · {summary}";
        return text.Length <= MaxTooltipLength
            ? text
            : string.Concat(text.AsSpan(0, MaxTooltipLength - 1), "…");
    }

    private static string StateName(TrayIconState state) => state switch
    {
        TrayIconState.Idle => "idle",
        TrayIconState.Busy => "busy",
        TrayIconState.Queued => "queued",
        TrayIconState.Down => "down",
        _ => throw new ArgumentOutOfRangeException(nameof(state)),
    };

    private static string ThemeName(TaskbarTheme theme) => theme switch
    {
        TaskbarTheme.Light => "light",
        TaskbarTheme.Dark => "dark",
        _ => throw new ArgumentOutOfRangeException(nameof(theme)),
    };
}
