namespace ResourcesHelper.Tray.Core.Flyout;

/// <summary>
/// A rectangle in physical screen pixels, the unit of window and monitor bounds.
/// </summary>
public readonly record struct PixelRect(int X, int Y, int Width, int Height)
{
    public int Right => X + Width;

    public int Bottom => Y + Height;
}
