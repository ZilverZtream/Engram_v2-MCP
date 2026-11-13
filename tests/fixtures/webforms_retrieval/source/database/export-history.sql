CREATE TABLE dbo.ExportHistory (
    ExportId int NOT NULL PRIMARY KEY,
    Username nvarchar(80) NOT NULL,
    PhotoCount int NOT NULL,
    CreatedAt datetime2 NOT NULL
);
