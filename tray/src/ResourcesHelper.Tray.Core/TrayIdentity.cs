namespace ResourcesHelper.Tray.Core;

public static class TrayIdentity
{
    /// <summary>
    /// Single-instance key shared by every build of the tray app. Changing it lets an upgraded
    /// build start next to a still-running old one, so it must stay fixed across releases.
    /// </summary>
    public static string InstanceKey { get; } = "ResourcesHelper.Tray";
}
