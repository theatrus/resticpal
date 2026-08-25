using Microsoft.UI.Xaml;
using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>One lazily loaded file or directory from the selected backup.</summary>
public sealed class RestoreBrowserEntryListItem
{
    internal RestoreBrowserEntryListItem(RestoreDirectoryEntry entry, bool sourceRoot = false)
    {
        Entry = entry;
        IsDirectory = RestorePresentation.IsDirectory(entry.Kind);
        SizeText = IsDirectory
            ? sourceRoot ? "Source folder" : "Folder"
            : entry.Size is ulong size
                ? RestorePresentation.FormatBytes(size)
                : "—";
        ModifiedAtText = entry.ModifiedAt is DateTimeOffset modified
            ? modified.ToLocalTime().ToString("g")
            : "—";
    }

    internal RestoreDirectoryEntry Entry { get; }

    public string Name => Entry.Name;

    public string Path => Entry.Path;

    public bool IsDirectory { get; }

    public string IconGlyph => IsDirectory ? "\uE8B7" : "\uE7C3";

    public Visibility OpenVisibility => IsDirectory ? Visibility.Visible : Visibility.Collapsed;

    public string SizeText { get; }

    public string ModifiedAtText { get; }
}
