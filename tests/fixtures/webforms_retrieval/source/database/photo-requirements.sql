CREATE TABLE dbo.PhotoRequirement (
    MarkerTypeId int NOT NULL,
    PhotoGroupId int NOT NULL,
    RequiredImageCount int NOT NULL CHECK (RequiredImageCount >= 0),
    CONSTRAINT PK_PhotoRequirement PRIMARY KEY (MarkerTypeId, PhotoGroupId)
);
