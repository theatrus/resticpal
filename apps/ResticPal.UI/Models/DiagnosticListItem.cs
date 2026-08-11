using ResticPal.UI.Services;

namespace ResticPal.UI;

/// <summary>Presentation model for one sanitized diagnostic record.</summary>
public sealed class DiagnosticListItem
{
    internal DiagnosticListItem(DiagnosticRecord record)
    {
        Headline = $"{record.Level.Replace('_', ' ')} · {record.EventId}";
        TimestampText = record.Timestamp.ToLocalTime().ToString("g");
        Message = record.Message;
        Detail = string.IsNullOrWhiteSpace(record.Code)
            ? "No error code."
            : $"Sanitized code: {record.Code}";
    }

    public string Headline { get; }
    public string TimestampText { get; }
    public string Message { get; }
    public string Detail { get; }
}
