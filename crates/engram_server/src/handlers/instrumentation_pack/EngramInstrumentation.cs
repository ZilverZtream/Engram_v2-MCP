using System;
using System.Globalization;

public static class EngramInstrumentation
{
    // Call explicitly at an observed event; supply your application's log sink.
    public static void LogEngramEvent(Action<string> sink, string sourcePath,
        string eventName, string controlId, string sqlNodeId)
    {
        if (sink == null) throw new ArgumentNullException("sink");
        sourcePath = Field(sourcePath, "sourcePath", false);
        eventName = Field(eventName, "eventName", false);
        controlId = Field(controlId, "controlId", true);
        sqlNodeId = Field(sqlNodeId, "sqlNodeId", false);
        const string prefix = "sql:inline:";
        if (!sqlNodeId.StartsWith(prefix, StringComparison.Ordinal) || sqlNodeId.Length == prefix.Length)
            throw new ArgumentException("Supply an existing sql:inline:<id> node; stored procedures are not supported by this log importer.", "sqlNodeId");
        sink(String.Join("|", new[] { "ENGRAM_LOG", DateTime.UtcNow.ToString("o", CultureInfo.InvariantCulture),
            sourcePath, eventName, controlId, sqlNodeId.Substring(prefix.Length) }));
    }

    private static string Field(string value, string name, bool allowEmpty)
    {
        value = value ?? String.Empty;
        if ((!allowEmpty && String.IsNullOrWhiteSpace(value)) || value.IndexOfAny(new[] { '|', '\r', '\n' }) >= 0)
            throw new ArgumentException("A log field must not contain a record delimiter or newline; required fields must not be empty.", name);
        return value;
    }
}
