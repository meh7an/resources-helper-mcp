namespace ResourcesHelper.Tray.Core.TrayIcon;

public enum TrayEvent
{
    Other,

    /// <summary>
    /// A left click (NIN_SELECT).
    /// </summary>
    Select,

    /// <summary>
    /// Enter or Space on the focused icon (NIN_KEYSELECT).
    /// </summary>
    KeySelect,

    /// <summary>
    /// A right click or Shift+F10 (WM_CONTEXTMENU).
    /// </summary>
    ContextMenu,
}

/// <summary>
/// One NOTIFYICON_VERSION_4 callback: the event in the low word of lParam, and the anchor
/// point in screen pixels packed into wParam.
/// </summary>
public readonly record struct TrayCallback(TrayEvent Event, int Message, int X, int Y)
{
    private const int WmContextMenu = 0x007B;
    private const int NinSelect = 0x0400;
    private const int NinKeySelect = 0x0401;

    public static TrayCallback Decode(nint wParam, nint lParam)
    {
        var message = (int)(lParam & 0xFFFF);
        var kind = message switch
        {
            NinSelect => TrayEvent.Select,
            NinKeySelect => TrayEvent.KeySelect,
            WmContextMenu => TrayEvent.ContextMenu,
            _ => TrayEvent.Other,
        };
        // Signed words: a monitor left of or above the primary one has negative coordinates.
        var x = (short)(wParam & 0xFFFF);
        var y = (short)((wParam >> 16) & 0xFFFF);
        return new TrayCallback(kind, message, x, y);
    }
}
