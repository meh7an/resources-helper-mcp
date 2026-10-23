namespace ResourcesHelper.Tray.Core.Tests;

[TestClass]
public sealed class TrayIdentityTests
{
    [TestMethod]
    public void InstanceKeyStaysFixedAcrossReleases() =>
        Assert.AreEqual("ResourcesHelper.Tray", TrayIdentity.InstanceKey);
}
