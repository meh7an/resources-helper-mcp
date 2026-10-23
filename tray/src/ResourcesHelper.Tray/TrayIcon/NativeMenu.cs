using System.Runtime.InteropServices;

using ResourcesHelper.Tray.Core.TrayIcon;
using ResourcesHelper.Tray.Interop;

namespace ResourcesHelper.Tray.TrayIcon;

internal sealed record NativeMenuItem(string Text, uint Id = 0)
{
    public static NativeMenuItem Separator { get; } = new(string.Empty) { IsSeparator = true };

    public bool IsSeparator { get; init; }

    public bool Enabled { get; init; } = true;

    public bool Checked { get; init; }

    /// <summary>
    /// Drawn bold; the item a click on the icon would also run.
    /// </summary>
    public bool IsDefault { get; init; }

    public IReadOnlyList<NativeMenuItem>? Children { get; init; }
}

/// <summary>
/// A Win32 popup menu for the tray icon, themed to match the taskbar.
/// </summary>
internal static unsafe class NativeMenu
{
    private const int SetPreferredAppModeOrdinal = 135;
    private const int FlushMenuThemesOrdinal = 136;
    private const int ForceDark = 2;
    private const int ForceLight = 3;

    // uxtheme's undocumented SetPreferredAppMode and FlushMenuThemes are the only way to get
    // dark Win32 menus (Explorer and H.NotifyIcon use them too). Missing exports mean light menus.
    private static readonly nint SetPreferredAppMode = UxThemeExport(SetPreferredAppModeOrdinal);
    private static readonly nint FlushMenuThemes = UxThemeExport(FlushMenuThemesOrdinal);

    /// <summary>
    /// Shows the menu with its bottom edge at the point, blocking until it closes.
    /// </summary>
    /// <returns>The chosen item's ID, or 0 when the menu was dismissed.</returns>
    public static uint Show(nint owner, IReadOnlyList<NativeMenuItem> items, int x, int y)
    {
        var menu = Build(items);
        try
        {
            // Without the owner in the foreground the menu never closes on an outside click,
            // and the WM_NULL afterwards lets the next right-click open it at once (KB135788).
            _ = NativeMethods.SetForegroundWindow(owner);
            var horizontal =
                NativeMethods.GetSystemMetrics(NativeMethods.SmMenuDropAlignment) != 0
                    ? NativeMethods.TpmRightAlign
                    : NativeMethods.TpmLeftAlign;
            var flags = NativeMethods.TpmReturnCmd
                | NativeMethods.TpmRightButton
                | NativeMethods.TpmBottomAlign
                | horizontal;
            var id = NativeMethods.TrackPopupMenuEx(menu, flags, x, y, owner, 0);
            _ = NativeMethods.PostMessage(owner, NativeMethods.WmNull, 0, 0);
            return (uint)id;
        }
        finally
        {
            // Also destroys the submenus attached to it.
            _ = NativeMethods.DestroyMenu(menu);
        }
    }

    /// <returns>False when this Windows build lacks the uxtheme exports.</returns>
    public static bool UseTheme(TaskbarTheme theme)
    {
        if (SetPreferredAppMode == 0 || FlushMenuThemes == 0)
        {
            return false;
        }

        _ = ((delegate* unmanaged<int, int>)SetPreferredAppMode)(
            theme == TaskbarTheme.Dark ? ForceDark : ForceLight);
        ((delegate* unmanaged<void>)FlushMenuThemes)();
        return true;
    }

    private static nint Build(IReadOnlyList<NativeMenuItem> items)
    {
        var menu = NativeMethods.CreatePopupMenu();
        foreach (var item in items)
        {
            if (item.IsSeparator)
            {
                _ = NativeMethods.AppendMenu(menu, NativeMethods.MfSeparator, 0, null);
                continue;
            }

            var flags = NativeMethods.MfString
                | (item.Enabled ? 0 : NativeMethods.MfGrayed)
                | (item.Checked ? NativeMethods.MfChecked : 0);
            if (item.Children is { } children)
            {
                _ = NativeMethods.AppendMenu(
                    menu, flags | NativeMethods.MfPopup, (nuint)Build(children), item.Text);
                continue;
            }

            _ = NativeMethods.AppendMenu(menu, flags, item.Id, item.Text);
            if (item.IsDefault)
            {
                _ = NativeMethods.SetMenuDefaultItem(menu, item.Id, 0);
            }
        }

        return menu;
    }

    private static nint UxThemeExport(int ordinal)
    {
        // Before Windows 10 1903 ordinal 135 was another function with another signature.
        if (!OperatingSystem.IsWindowsVersionAtLeast(10, 0, 18362))
        {
            return 0;
        }

        var loaded = NativeLibrary.TryLoad(
            "uxtheme.dll",
            typeof(NativeMenu).Assembly,
            DllImportSearchPath.System32,
            out var module);
        return loaded ? NativeMethods.GetProcAddress(module, ordinal) : 0;
    }
}
