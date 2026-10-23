using ResourcesHelper.Tray.Core.Flyout;

namespace ResourcesHelper.Tray.Core.Tests;

[TestClass]
public sealed class FlyoutPlacementTests
{
    private static readonly PixelRect Monitor1080 = new(0, 0, 1920, 1080);

    [TestMethod]
    public void SitsTwelvePixelsFromBottomRightAtOneHundredPercent()
    {
        var workArea = new PixelRect(0, 0, 1920, 1032);

        var rect = FlyoutPlacement.TrayCorner(workArea, Monitor1080, 1.0, 400);

        Assert.AreEqual(new PixelRect(1920 - 12 - 360, 1032 - 12 - 400, 360, 400), rect);
    }

    [TestMethod]
    public void ScalesSizeAndMarginAtOneHundredFiftyPercent()
    {
        var monitor = new PixelRect(0, 0, 2560, 1440);
        var workArea = new PixelRect(0, 0, 2560, 1368);

        var rect = FlyoutPlacement.TrayCorner(workArea, monitor, 1.5, 400);

        Assert.AreEqual(new PixelRect(2560 - 18 - 540, 1368 - 18 - 600, 540, 600), rect);
    }

    [TestMethod]
    public void MirrorsToTopRightWhenTaskbarIsAtTop()
    {
        var workArea = new PixelRect(0, 48, 1920, 1032);

        var rect = FlyoutPlacement.TrayCorner(workArea, Monitor1080, 1.0, 400);

        Assert.AreEqual(new PixelRect(1920 - 12 - 360, 48 + 12, 360, 400), rect);
    }

    [TestMethod]
    public void UsesWorkAreaOffsetOfNonOriginMonitor()
    {
        var monitor = new PixelRect(-1920, 200, 1920, 1080);
        var workArea = new PixelRect(-1920, 200, 1920, 1032);

        var rect = FlyoutPlacement.TrayCorner(workArea, monitor, 1.0, 300);

        Assert.AreEqual(new PixelRect(-12 - 360, 200 + 1032 - 12 - 300, 360, 300), rect);
    }

    [TestMethod]
    public void TreatsAutoHideTaskbarAsBottom() =>
        Assert.IsFalse(FlyoutPlacement.IsTaskbarAtTop(Monitor1080, Monitor1080));

    [TestMethod]
    public void TreatsLeftTaskbarAsBottom() =>
        Assert.IsFalse(
            FlyoutPlacement.IsTaskbarAtTop(new PixelRect(48, 0, 1872, 1080), Monitor1080));

    [TestMethod]
    public void CapsHeightAtSixHundredForty() =>
        Assert.AreEqual(640, FlyoutPlacement.ClampHeight(900, 1400));

    [TestMethod]
    public void CapsHeightAtSeventyPercentOfShortWorkArea() =>
        Assert.AreEqual(560, FlyoutPlacement.ClampHeight(900, 800), 0.001);

    [TestMethod]
    public void KeepsContentHeightWhenItFits() =>
        Assert.AreEqual(320, FlyoutPlacement.ClampHeight(320, 1032));

    [TestMethod]
    public void ClampsHeightInEffectivePixelsOfTheMonitor()
    {
        // 1368 physical at 150% is 912 effective; 70% of that (638.4) is below 640.
        var monitor = new PixelRect(0, 0, 2560, 1440);
        var workArea = new PixelRect(0, 0, 2560, 1368);

        var rect = FlyoutPlacement.TrayCorner(workArea, monitor, 1.5, 900);

        Assert.AreEqual(FlyoutPlacement.ToPixels(638.4, 1.5), rect.Height);
    }
}
