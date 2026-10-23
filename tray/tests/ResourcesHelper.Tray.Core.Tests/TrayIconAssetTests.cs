using ResourcesHelper.Tray.Core.TrayIcon;

namespace ResourcesHelper.Tray.Core.Tests;

[TestClass]
public sealed class TrayIconAssetTests
{
    [TestMethod]
    [DataRow(TrayIconState.Idle, TaskbarTheme.Light, "rh-idle-on-light-taskbar.ico")]
    [DataRow(TrayIconState.Busy, TaskbarTheme.Dark, "rh-busy-on-dark-taskbar.ico")]
    [DataRow(TrayIconState.Queued, TaskbarTheme.Light, "rh-queued-on-light-taskbar.ico")]
    [DataRow(TrayIconState.Down, TaskbarTheme.Dark, "rh-down-on-dark-taskbar.ico")]
    public void FileNameMatchesTheHandoffAssets(
        TrayIconState state, TaskbarTheme theme, string expected) =>
        Assert.AreEqual(expected, TrayIconAsset.FileName(state, theme));

    [TestMethod]
    [DataRow(1, TaskbarTheme.Light)]
    [DataRow(0, TaskbarTheme.Dark)]
    [DataRow(null, TaskbarTheme.Dark)]
    public void ThemeFollowsTheTaskbarSetting(int? value, TaskbarTheme expected) =>
        Assert.AreEqual(expected, TrayIconAsset.ThemeFrom(value));

    [TestMethod]
    public void TooltipNamesTheApp() =>
        Assert.AreEqual(
            "Resources Helper · 2 running · 1 waiting",
            TrayIconAsset.Tooltip("2 running · 1 waiting"));

    [TestMethod]
    public void LongTooltipFitsTheShellLimit()
    {
        var tooltip = TrayIconAsset.Tooltip(new string('x', 200));

        Assert.HasCount(TrayIconAsset.MaxTooltipLength, tooltip);
        Assert.EndsWith("…", tooltip);
    }
}
