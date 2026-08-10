using System.Reflection;

namespace ResticPal.UI.Services;

internal static class UpdateTrust
{
    internal const string AppCastUrl =
        "https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml";

    internal static string PublicKey { get; } = LoadPublicKey();

    private static string LoadPublicKey()
    {
        using Stream stream = typeof(UpdateTrust).Assembly.GetManifestResourceStream(
            "ResticPal.UpdatePublicKey")
            ?? throw new InvalidOperationException("The updater public key is missing.");
        using var reader = new StreamReader(stream);
        string value = reader.ReadToEnd().Trim();
        byte[] key;
        try
        {
            key = Convert.FromBase64String(value);
        }
        catch (FormatException exception)
        {
            throw new InvalidOperationException("The updater public key is malformed.", exception);
        }

        if (key.Length != 32)
        {
            throw new InvalidOperationException("The updater public key must contain 32 bytes.");
        }

        return value;
    }
}
