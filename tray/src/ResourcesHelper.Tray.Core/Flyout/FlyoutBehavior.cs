namespace ResourcesHelper.Tray.Core.Flyout;

public enum ToggleResult
{
    Show,
    Hide,
    Ignore,
}

/// <summary>
/// The open, light-dismiss, Stay open and Pin rules from the handoff, free of any window API.
/// Times are monotonic milliseconds supplied by the caller.
/// </summary>
public sealed class FlyoutBehavior(TimeSpan toggleDebounce)
{
    private long? _lightDismissedAtMs;

    public bool IsOpen { get; private set; }

    /// <summary>
    /// Turns light dismiss and Esc off.
    /// </summary>
    public bool StayOpen { get; private set; }

    /// <summary>
    /// Topmost; exists only while Stay open is on.
    /// </summary>
    public bool Pinned { get; private set; }

    /// <summary>
    /// A menu inside the flyout takes activation, which must not count as a dismiss.
    /// </summary>
    public bool MenuOpen { get; set; }

    /// <summary>
    /// A tray click toggles the flyout. The click itself deactivates the
    /// flyout first, so a toggle right after a light dismiss is that same click and must not
    /// reopen it.
    /// </summary>
    public ToggleResult Toggle(long nowMs)
    {
        if (IsOpen)
        {
            Close();
            return ToggleResult.Hide;
        }

        if (_lightDismissedAtMs is { } dismissedAt && nowMs - dismissedAt < DebounceMs)
        {
            _lightDismissedAtMs = null;
            return ToggleResult.Ignore;
        }

        IsOpen = true;
        return ToggleResult.Show;
    }

    /// <summary>
    /// A menu command, a toast or a second launch opens the flyout. None of them is the tray
    /// click that may just have dismissed it, so the debounce doesn't apply.
    /// </summary>
    /// <returns>True when the flyout should show.</returns>
    public bool Open()
    {
        if (IsOpen)
        {
            return false;
        }

        IsOpen = true;
        _lightDismissedAtMs = null;
        return true;
    }

    /// <summary>
    /// The window lost activation.
    /// </summary>
    /// <returns>True when the flyout should hide.</returns>
    public bool Deactivated(long nowMs)
    {
        if (!IsOpen || StayOpen || MenuOpen)
        {
            return false;
        }

        IsOpen = false;
        _lightDismissedAtMs = nowMs;
        return true;
    }

    /// <returns>True when the flyout should hide.</returns>
    public bool Escape()
    {
        if (!IsOpen || StayOpen || MenuOpen)
        {
            return false;
        }

        IsOpen = false;
        return true;
    }

    public void SetStayOpen(bool on)
    {
        StayOpen = on;
        if (!on)
        {
            Pinned = false;
        }
    }

    public void SetPinned(bool on) => Pinned = on && StayOpen;

    private long DebounceMs => (long)toggleDebounce.TotalMilliseconds;

    /// <summary>
    /// Closing from the tray resets both toggles, so the flyout always reopens in light-dismiss
    /// mode.
    /// </summary>
    private void Close()
    {
        IsOpen = false;
        StayOpen = false;
        Pinned = false;
        MenuOpen = false;
    }
}
