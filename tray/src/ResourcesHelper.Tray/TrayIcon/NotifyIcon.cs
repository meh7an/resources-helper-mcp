using System.ComponentModel;
using System.Runtime.InteropServices;

using Microsoft.Win32;

using ResourcesHelper.Tray.Core.TrayIcon;
using ResourcesHelper.Tray.Interop;

namespace ResourcesHelper.Tray.TrayIcon;

/// <summary>
/// The notification-area icon. A hidden window owns the Shell_NotifyIcon entry, picks the ICO
/// frame for the taskbar theme and the primary monitor's DPI, and adds the icon again when
/// Explorer restarts. Events are raised inside the window procedure, on the UI thread.
/// </summary>
internal sealed unsafe class NotifyIcon : IDisposable
{
    private const string WindowClassName = "ResourcesHelper.Tray.NotifyIcon";
    private const string PersonalizeKey =
        @"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    private const uint IconId = 1;
    private const uint CallbackMessage = NativeMethods.WmApp + 1;

    // The window procedure is a static callback, so it finds the icon here; there is only one.
    private static NotifyIcon? s_current;

    private readonly uint _taskbarCreated;
    private readonly nint _hwnd;
    private nint _icon;
    private TrayIconState _state;
    private TaskbarTheme _theme;
    private uint _dpi;
    private string _tooltip;

    public NotifyIcon(TrayIconState state, string tooltip)
    {
        if (s_current is not null)
        {
            throw new InvalidOperationException("Only one tray icon can exist.");
        }

        _state = state;
        _tooltip = tooltip;
        _taskbarCreated = NativeMethods.RegisterWindowMessage("TaskbarCreated");
        s_current = this;
        _hwnd = CreateHiddenWindow();
        _theme = ReadTaskbarTheme();
        _dpi = PrimaryDpi();
    }

    /// <summary>
    /// A left click, or Enter or Space on the focused icon.
    /// </summary>
    public event Action<TrayCallback>? Selected;

    public event Action<TrayCallback>? ContextMenuRequested;

    public event Action<TaskbarTheme>? ThemeChanged;

    public event Action<string>? Diagnostic;

    /// <summary>
    /// The hidden window, which owns the context menu.
    /// </summary>
    public nint WindowHandle => _hwnd;

    public TaskbarTheme Theme => _theme;

    /// <summary>
    /// Adds the icon; separate from the constructor so the events are attached first.
    /// </summary>
    public void Show()
    {
        ReloadIcon(notify: false);
        Add();
    }

    public void SetState(TrayIconState state)
    {
        if (state == _state)
        {
            return;
        }

        _state = state;
        ReloadIcon(notify: true);
    }

    public void SetTooltip(string tooltip)
    {
        _tooltip = tooltip;
        Update(NativeMethods.NifTip | NativeMethods.NifShowTip);
    }

    public void Dispose()
    {
        if (s_current != this)
        {
            return;
        }

        var data = NewData(0);
        _ = NativeMethods.ShellNotifyIcon(NativeMethods.NimDelete, &data);
        if (_icon != 0)
        {
            _ = NativeMethods.DestroyIcon(_icon);
            _icon = 0;
        }

        _ = NativeMethods.DestroyWindow(_hwnd);
        s_current = null;
    }

    private static nint CreateHiddenWindow()
    {
        var instance = NativeMethods.GetModuleHandle(null);
        fixed (char* className = WindowClassName)
        {
            var windowClass = new NativeMethods.WindowClass
            {
                Size = (uint)sizeof(NativeMethods.WindowClass),
                WindowProc = &WindowProc,
                Instance = instance,
                ClassName = className,
            };
            if (NativeMethods.RegisterClassEx(&windowClass) == 0)
            {
                throw new Win32Exception(Marshal.GetLastPInvokeError());
            }
        }

        // A message-only window never receives the TaskbarCreated and WM_SETTINGCHANGE
        // broadcasts; a hidden top-level window does.
        var hwnd = NativeMethods.CreateWindowEx(
            NativeMethods.WsExToolWindow,
            WindowClassName,
            "Resources Helper",
            NativeMethods.WsPopup,
            0,
            0,
            0,
            0,
            0,
            0,
            instance,
            0);
        return hwnd != 0 ? hwnd : throw new Win32Exception(Marshal.GetLastPInvokeError());
    }

    [UnmanagedCallersOnly]
    private static nint WindowProc(nint hwnd, uint message, nint wParam, nint lParam)
    {
        // An exception must not unwind into native code, which would end the process.
        try
        {
            if (s_current is { } icon && icon.HandleMessage(message, wParam, lParam))
            {
                return 0;
            }
        }
        catch (Exception e)
        {
            s_current?.Report($"window procedure failed: {e}");
        }

        return NativeMethods.DefWindowProc(hwnd, message, wParam, lParam);
    }

    private static TaskbarTheme ReadTaskbarTheme()
    {
        using var key = Registry.CurrentUser.OpenSubKey(PersonalizeKey);
        return TrayIconAsset.ThemeFrom(key?.GetValue("SystemUsesLightTheme") as int?);
    }

    // The notification area lives on the primary monitor's taskbar.
    private static uint PrimaryDpi()
    {
        var monitor = NativeMethods.MonitorFromPoint(
            default, NativeMethods.MonitorDefaultToPrimary);
        var hr = NativeMethods.GetDpiForMonitor(
            monitor, NativeMethods.MdtEffectiveDpi, out var dpi, out _);
        return hr == 0 ? dpi : 96;
    }

    private static void CopyTip(ref NativeMethods.NotifyIconData data, string text)
    {
        var length = Math.Min(text.Length, TrayIconAsset.MaxTooltipLength);
        for (var i = 0; i < length; i++)
        {
            data.Tip[i] = text[i];
        }

        data.Tip[length] = '\0';
    }

    /// <returns>True when the message is fully handled.</returns>
    private bool HandleMessage(uint message, nint wParam, nint lParam)
    {
        if (message == CallbackMessage)
        {
            OnCallback(TrayCallback.Decode(wParam, lParam));
            return true;
        }

        if (_taskbarCreated != 0 && message == _taskbarCreated)
        {
            OnTaskbarCreated();
            return true;
        }

        // M0.3 logs which of these arrive on a theme or scaling change; Refresh is cheap.
        switch (message)
        {
            case NativeMethods.WmSettingChange:
                var area = lParam == 0 ? string.Empty : Marshal.PtrToStringUni(lParam);
                Refresh($"WM_SETTINGCHANGE '{area}'");
                break;
            case NativeMethods.WmDisplayChange:
                Refresh("WM_DISPLAYCHANGE");
                break;
            case NativeMethods.WmDpiChanged:
                Refresh("WM_DPICHANGED");
                break;
        }

        return false;
    }

    private void OnCallback(TrayCallback callback)
    {
        switch (callback.Event)
        {
            case TrayEvent.Select or TrayEvent.KeySelect:
                Report($"{callback.Event} at {callback.X},{callback.Y}");
                Selected?.Invoke(callback);
                break;
            case TrayEvent.ContextMenu:
                Report($"{callback.Event} at {callback.X},{callback.Y}");
                ContextMenuRequested?.Invoke(callback);
                break;
            case TrayEvent.Other:
                break;
        }
    }

    private void OnTaskbarCreated()
    {
        var previousTheme = _theme;
        _theme = ReadTaskbarTheme();
        _dpi = PrimaryDpi();
        Report($"TaskbarCreated: theme {_theme}, dpi {_dpi}");
        ReloadIcon(notify: false);
        // The message also arrives when the taskbar only changed, with the icon still there.
        var data = NewData(0);
        _ = NativeMethods.ShellNotifyIcon(NativeMethods.NimDelete, &data);
        Add();
        if (_theme != previousTheme)
        {
            ThemeChanged?.Invoke(_theme);
        }
    }

    private void Refresh(string reason)
    {
        var theme = ReadTaskbarTheme();
        var dpi = PrimaryDpi();
        if (theme == _theme && dpi == _dpi)
        {
            Report($"{reason}: unchanged");
            return;
        }

        var themeChanged = theme != _theme;
        _theme = theme;
        _dpi = dpi;
        Report($"{reason}: theme {theme}, dpi {dpi}");
        ReloadIcon(notify: true);
        if (themeChanged)
        {
            ThemeChanged?.Invoke(theme);
        }
    }

    private void Add()
    {
        var data = NewData(
            NativeMethods.NifMessage
                | NativeMethods.NifIcon
                | NativeMethods.NifTip
                | NativeMethods.NifShowTip);
        if (!NativeMethods.ShellNotifyIcon(NativeMethods.NimAdd, &data))
        {
            // Early in a logon the taskbar may not exist yet; TaskbarCreated retries.
            Report("add failed; waiting for TaskbarCreated");
            return;
        }

        data.TimeoutOrVersion = NativeMethods.NotifyIconVersion4;
        var versioned = NativeMethods.ShellNotifyIcon(NativeMethods.NimSetVersion, &data);
        Report($"added ({sizeof(NativeMethods.NotifyIconData)} byte data), version 4 {versioned}");
    }

    private void Update(uint flags)
    {
        var data = NewData(flags);
        if (!NativeMethods.ShellNotifyIcon(NativeMethods.NimModify, &data))
        {
            Report($"modify 0x{flags:X} failed");
        }
    }

    private void ReloadIcon(bool notify)
    {
        var size = NativeMethods.GetSystemMetricsForDpi(NativeMethods.SmCxSmIcon, _dpi);
        var file = TrayIconAsset.FileName(_state, _theme);
        var path = Path.Combine(AppContext.BaseDirectory, "Assets", "Tray", file);
        var icon = NativeMethods.LoadImage(
            0, path, NativeMethods.ImageIcon, size, size, NativeMethods.LrLoadFromFile);
        if (icon == 0)
        {
            Report($"loading {path} failed (error {Marshal.GetLastPInvokeError()})");
            return;
        }

        var previous = _icon;
        _icon = icon;
        if (notify)
        {
            Update(NativeMethods.NifIcon);
        }

        // The shell keeps its own copy, so the old handle can go once the new one is set.
        if (previous != 0)
        {
            _ = NativeMethods.DestroyIcon(previous);
        }

        Report($"icon {file} at {size} px (dpi {_dpi})");
    }

    // Identified by window and ID, not a GUID: an unpackaged app's GUID is bound to the exe
    // path, so the same GUID from another build folder fails to add.
    private NativeMethods.NotifyIconData NewData(uint flags)
    {
        var data = new NativeMethods.NotifyIconData
        {
            Size = (uint)sizeof(NativeMethods.NotifyIconData),
            Window = _hwnd,
            Id = IconId,
            Flags = flags,
            CallbackMessage = CallbackMessage,
            Icon = _icon,
        };
        CopyTip(ref data, _tooltip);
        return data;
    }

    private void Report(string message) => Diagnostic?.Invoke(message);
}
