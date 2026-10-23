using ResourcesHelper.Tray.Core.Flyout;

namespace ResourcesHelper.Tray.Core.Tests;

[TestClass]
public sealed class FlyoutBehaviorTests
{
    private static FlyoutBehavior Opened()
    {
        var behavior = new FlyoutBehavior(TimeSpan.FromMilliseconds(500));
        _ = behavior.Toggle(0);
        return behavior;
    }

    [TestMethod]
    public void ToggleOpensThenCloses()
    {
        var behavior = new FlyoutBehavior(TimeSpan.FromMilliseconds(500));

        Assert.AreEqual(ToggleResult.Show, behavior.Toggle(0));
        Assert.AreEqual(ToggleResult.Hide, behavior.Toggle(1000));
        Assert.IsFalse(behavior.IsOpen);
    }

    [TestMethod]
    public void DeactivationDismisses()
    {
        var behavior = Opened();

        Assert.IsTrue(behavior.Deactivated(100));
        Assert.IsFalse(behavior.IsOpen);
    }

    [TestMethod]
    public void ToggleRightAfterLightDismissIsTheSameClick()
    {
        var behavior = Opened();
        _ = behavior.Deactivated(1000);

        Assert.AreEqual(ToggleResult.Ignore, behavior.Toggle(1499));
        Assert.IsFalse(behavior.IsOpen);
    }

    [TestMethod]
    public void ToggleAfterTheDebounceReopens()
    {
        var behavior = Opened();
        _ = behavior.Deactivated(1000);

        Assert.AreEqual(ToggleResult.Show, behavior.Toggle(1500));
    }

    [TestMethod]
    public void DebounceIsConsumedByOneToggle()
    {
        var behavior = Opened();
        _ = behavior.Deactivated(1000);
        _ = behavior.Toggle(1100);

        Assert.AreEqual(ToggleResult.Show, behavior.Toggle(1200));
    }

    [TestMethod]
    public void OpenIgnoresTheDebounce()
    {
        var behavior = Opened();
        _ = behavior.Deactivated(1000);

        Assert.IsTrue(behavior.Open());
        Assert.IsTrue(behavior.IsOpen);
    }

    [TestMethod]
    public void OpenWhileOpenDoesNothing()
    {
        var behavior = Opened();

        Assert.IsFalse(behavior.Open());
        Assert.IsTrue(behavior.IsOpen);
    }

    [TestMethod]
    public void EscapeDismisses()
    {
        var behavior = Opened();

        Assert.IsTrue(behavior.Escape());
        Assert.IsFalse(behavior.IsOpen);
    }

    [TestMethod]
    public void StayOpenTurnsOffLightDismissAndEscape()
    {
        var behavior = Opened();
        behavior.SetStayOpen(true);

        Assert.IsFalse(behavior.Deactivated(100));
        Assert.IsFalse(behavior.Escape());
        Assert.IsTrue(behavior.IsOpen);
    }

    [TestMethod]
    public void OpenMenuSuppressesDismiss()
    {
        var behavior = Opened();
        behavior.MenuOpen = true;

        Assert.IsFalse(behavior.Deactivated(100));
        Assert.IsFalse(behavior.Escape());

        behavior.MenuOpen = false;
        Assert.IsTrue(behavior.Deactivated(200));
    }

    [TestMethod]
    public void PinNeedsStayOpen()
    {
        var behavior = Opened();

        behavior.SetPinned(true);

        Assert.IsFalse(behavior.Pinned);
    }

    [TestMethod]
    public void TurningStayOpenOffAlsoUnpins()
    {
        var behavior = Opened();
        behavior.SetStayOpen(true);
        behavior.SetPinned(true);

        behavior.SetStayOpen(false);

        Assert.IsFalse(behavior.Pinned);
    }

    [TestMethod]
    public void ClosingFromTheTrayResetsBothToggles()
    {
        var behavior = Opened();
        behavior.SetStayOpen(true);
        behavior.SetPinned(true);

        _ = behavior.Toggle(100);
        _ = behavior.Toggle(200);

        Assert.IsTrue(behavior.IsOpen);
        Assert.IsFalse(behavior.StayOpen);
        Assert.IsFalse(behavior.Pinned);
    }
}
