using ResourcesHelper.Tray.Core.TrayIcon;

namespace ResourcesHelper.Tray.Core.Tests;

[TestClass]
public sealed class TrayCallbackTests
{
    private static nint Point(int x, int y) => (nint)(((y & 0xFFFF) << 16) | (x & 0xFFFF));

    // The icon ID sits in the high word of lParam.
    private static nint Event(int message) => (1 << 16) | message;

    [TestMethod]
    [DataRow(0x0400, TrayEvent.Select)]
    [DataRow(0x0401, TrayEvent.KeySelect)]
    [DataRow(0x007B, TrayEvent.ContextMenu)]
    [DataRow(0x0200, TrayEvent.Other)]
    public void EventComesFromTheLowWord(int message, TrayEvent expected)
    {
        var callback = TrayCallback.Decode(Point(10, 20), Event(message));

        Assert.AreEqual(expected, callback.Event);
        Assert.AreEqual(message, callback.Message);
    }

    [TestMethod]
    public void AnchorPointComesFromWParam()
    {
        var callback = TrayCallback.Decode(Point(1890, 1060), Event(0x007B));

        Assert.AreEqual(1890, callback.X);
        Assert.AreEqual(1060, callback.Y);
    }

    [TestMethod]
    public void AnchorPointKeepsNegativeCoordinates()
    {
        var callback = TrayCallback.Decode(Point(-100, -5), Event(0x0400));

        Assert.AreEqual(-100, callback.X);
        Assert.AreEqual(-5, callback.Y);
    }
}
