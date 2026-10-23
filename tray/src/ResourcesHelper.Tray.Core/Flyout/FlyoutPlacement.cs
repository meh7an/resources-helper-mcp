namespace ResourcesHelper.Tray.Core.Flyout;

/// <summary>
/// Where the flyout sits next to the taskbar. Sizes are effective pixels; the result is in
/// physical pixels for the monitor whose scale is given.
/// </summary>
public static class FlyoutPlacement
{
    public const double Width = 360;
    public const double MaxHeight = 640;
    public const double MaxWorkAreaFraction = 0.7;
    public const double EdgeMargin = 12;

    /// <summary>
    /// The flyout height for its content: up to 640, or 70% of the work area if that is smaller.
    /// </summary>
    public static double ClampHeight(double contentHeight, double workAreaHeight) =>
        Math.Min(contentHeight, Math.Min(MaxHeight, workAreaHeight * MaxWorkAreaFraction));

    /// <summary>
    /// The tray-corner rectangle: 12 px from the right and bottom of the work area, or from the
    /// right and top when the taskbar sits at the top of the monitor.
    /// </summary>
    public static PixelRect TrayCorner(
        PixelRect workArea,
        PixelRect monitorBounds,
        double scale,
        double contentHeight)
    {
        var height = ClampHeight(contentHeight, workArea.Height / scale);
        var widthPx = ToPixels(Width, scale);
        var heightPx = ToPixels(height, scale);
        var marginPx = ToPixels(EdgeMargin, scale);

        var x = workArea.Right - marginPx - widthPx;
        var y = IsTaskbarAtTop(workArea, monitorBounds)
            ? workArea.Y + marginPx
            : workArea.Bottom - marginPx - heightPx;
        return new PixelRect(x, y, widthPx, heightPx);
    }

    /// <summary>
    /// The work area starts below the monitor's top edge only when an appbar (the taskbar, moved
    /// by a third-party tool) occupies the top. An auto-hide taskbar leaves them equal, which
    /// counts as bottom.
    /// </summary>
    public static bool IsTaskbarAtTop(PixelRect workArea, PixelRect monitorBounds) =>
        workArea.Y > monitorBounds.Y && workArea.Bottom == monitorBounds.Bottom;

    public static int ToPixels(double effective, double scale) =>
        (int)Math.Round(effective * scale, MidpointRounding.AwayFromZero);
}
