/* A Bison parser, made by GNU Bison 3.8.2.  */

/* Bison interface for Yacc-like parsers in C

   Copyright (C) 1984, 1989-1990, 2000-2015, 2018-2021 Free Software Foundation,
   Inc.

   This program is free software: you can redistribute it and/or modify
   it under the terms of the GNU General Public License as published by
   the Free Software Foundation, either version 3 of the License, or
   (at your option) any later version.

   This program is distributed in the hope that it will be useful,
   but WITHOUT ANY WARRANTY; without even the implied warranty of
   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
   GNU General Public License for more details.

   You should have received a copy of the GNU General Public License
   along with this program.  If not, see <https://www.gnu.org/licenses/>.  */

/* As a special exception, you may create a larger work that contains
   part or all of the Bison parser skeleton and distribute that work
   under terms of your choice, so long as that work isn't itself a
   parser generator using the skeleton or a modified version thereof
   as a parser skeleton.  Alternatively, if you modify or redistribute
   the parser skeleton itself, you may (at your option) remove this
   special exception, which will cause the skeleton and the resulting
   Bison output files to be licensed under the GNU General Public
   License without this special exception.

   This special exception was added by the Free Software Foundation in
   version 2.2 of Bison.  */

/* DO NOT RELY ON FEATURES THAT ARE NOT DOCUMENTED in the manual,
   especially those whose name start with YY_ or yy_.  They are
   private implementation details that can be changed or removed.  */

#ifndef YY_BASE_YY_GRAM_H_INCLUDED
# define YY_BASE_YY_GRAM_H_INCLUDED
/* Debug traces.  */
#ifndef YYDEBUG
# define YYDEBUG 0
#endif
#if YYDEBUG
extern int base_yydebug;
#endif

/* Token kinds.  */
#ifndef YYTOKENTYPE
# define YYTOKENTYPE
  enum yytokentype
  {
    YYEMPTY = -2,
    YYEOF = 0,                     /* "end of file"  */
    YYerror = 256,                 /* error  */
    YYUNDEF = 257,                 /* "invalid token"  */
    IDENT = 258,                   /* IDENT  */
    UIDENT = 259,                  /* UIDENT  */
    FCONST = 260,                  /* FCONST  */
    SCONST = 261,                  /* SCONST  */
    USCONST = 262,                 /* USCONST  */
    BCONST = 263,                  /* BCONST  */
    XCONST = 264,                  /* XCONST  */
    Op = 265,                      /* Op  */
    ICONST = 266,                  /* ICONST  */
    PARAM = 267,                   /* PARAM  */
    TYPECAST = 268,                /* TYPECAST  */
    DOT_DOT = 269,                 /* DOT_DOT  */
    COLON_EQUALS = 270,            /* COLON_EQUALS  */
    EQUALS_GREATER = 271,          /* EQUALS_GREATER  */
    LESS_EQUALS = 272,             /* LESS_EQUALS  */
    GREATER_EQUALS = 273,          /* GREATER_EQUALS  */
    NOT_EQUALS = 274,              /* NOT_EQUALS  */
    ABORT_P = 275,                 /* ABORT_P  */
    ABSENT = 276,                  /* ABSENT  */
    ABSOLUTE_P = 277,              /* ABSOLUTE_P  */
    ACCESS = 278,                  /* ACCESS  */
    ACTION = 279,                  /* ACTION  */
    ADD_P = 280,                   /* ADD_P  */
    ADMIN = 281,                   /* ADMIN  */
    AFTER = 282,                   /* AFTER  */
    AGGREGATE = 283,               /* AGGREGATE  */
    ALL = 284,                     /* ALL  */
    ALSO = 285,                    /* ALSO  */
    ALTER = 286,                   /* ALTER  */
    ALWAYS = 287,                  /* ALWAYS  */
    ANALYSE = 288,                 /* ANALYSE  */
    ANALYZE = 289,                 /* ANALYZE  */
    AND = 290,                     /* AND  */
    ANY = 291,                     /* ANY  */
    ARRAY = 292,                   /* ARRAY  */
    AS = 293,                      /* AS  */
    ASC = 294,                     /* ASC  */
    ASENSITIVE = 295,              /* ASENSITIVE  */
    ASSERTION = 296,               /* ASSERTION  */
    ASSIGNMENT = 297,              /* ASSIGNMENT  */
    ASYMMETRIC = 298,              /* ASYMMETRIC  */
    ATOMIC = 299,                  /* ATOMIC  */
    AT = 300,                      /* AT  */
    ATTACH = 301,                  /* ATTACH  */
    ATTRIBUTE = 302,               /* ATTRIBUTE  */
    AUTHORIZATION = 303,           /* AUTHORIZATION  */
    BACKWARD = 304,                /* BACKWARD  */
    BEFORE = 305,                  /* BEFORE  */
    BEGIN_P = 306,                 /* BEGIN_P  */
    BETWEEN = 307,                 /* BETWEEN  */
    BIGINT = 308,                  /* BIGINT  */
    BINARY = 309,                  /* BINARY  */
    BIT = 310,                     /* BIT  */
    BOOLEAN_P = 311,               /* BOOLEAN_P  */
    BOTH = 312,                    /* BOTH  */
    BREADTH = 313,                 /* BREADTH  */
    BY = 314,                      /* BY  */
    CACHE = 315,                   /* CACHE  */
    CALL = 316,                    /* CALL  */
    CALLED = 317,                  /* CALLED  */
    CASCADE = 318,                 /* CASCADE  */
    CASCADED = 319,                /* CASCADED  */
    CASE = 320,                    /* CASE  */
    CAST = 321,                    /* CAST  */
    CATALOG_P = 322,               /* CATALOG_P  */
    CHAIN = 323,                   /* CHAIN  */
    CHAR_P = 324,                  /* CHAR_P  */
    CHARACTER = 325,               /* CHARACTER  */
    CHARACTERISTICS = 326,         /* CHARACTERISTICS  */
    CHECK = 327,                   /* CHECK  */
    CHECKPOINT = 328,              /* CHECKPOINT  */
    CLASS = 329,                   /* CLASS  */
    CLOSE = 330,                   /* CLOSE  */
    CLUSTER = 331,                 /* CLUSTER  */
    COALESCE = 332,                /* COALESCE  */
    COLLATE = 333,                 /* COLLATE  */
    COLLATION = 334,               /* COLLATION  */
    COLUMN = 335,                  /* COLUMN  */
    COLUMNS = 336,                 /* COLUMNS  */
    COMMENT = 337,                 /* COMMENT  */
    COMMENTS = 338,                /* COMMENTS  */
    COMMIT = 339,                  /* COMMIT  */
    COMMITTED = 340,               /* COMMITTED  */
    COMPRESSION = 341,             /* COMPRESSION  */
    CONCURRENTLY = 342,            /* CONCURRENTLY  */
    CONDITIONAL = 343,             /* CONDITIONAL  */
    CONFIGURATION = 344,           /* CONFIGURATION  */
    CONFLICT = 345,                /* CONFLICT  */
    CONNECTION = 346,              /* CONNECTION  */
    CONSTRAINT = 347,              /* CONSTRAINT  */
    CONSTRAINTS = 348,             /* CONSTRAINTS  */
    CONTENT_P = 349,               /* CONTENT_P  */
    CONTINUE_P = 350,              /* CONTINUE_P  */
    CONVERSION_P = 351,            /* CONVERSION_P  */
    COPY = 352,                    /* COPY  */
    COST = 353,                    /* COST  */
    CREATE = 354,                  /* CREATE  */
    CROSS = 355,                   /* CROSS  */
    CSV = 356,                     /* CSV  */
    CUBE = 357,                    /* CUBE  */
    CURRENT_P = 358,               /* CURRENT_P  */
    CURRENT_CATALOG = 359,         /* CURRENT_CATALOG  */
    CURRENT_DATE = 360,            /* CURRENT_DATE  */
    CURRENT_ROLE = 361,            /* CURRENT_ROLE  */
    CURRENT_SCHEMA = 362,          /* CURRENT_SCHEMA  */
    CURRENT_TIME = 363,            /* CURRENT_TIME  */
    CURRENT_TIMESTAMP = 364,       /* CURRENT_TIMESTAMP  */
    CURRENT_USER = 365,            /* CURRENT_USER  */
    CURSOR = 366,                  /* CURSOR  */
    CYCLE = 367,                   /* CYCLE  */
    DATA_P = 368,                  /* DATA_P  */
    DATABASE = 369,                /* DATABASE  */
    DAY_P = 370,                   /* DAY_P  */
    DEALLOCATE = 371,              /* DEALLOCATE  */
    DEC = 372,                     /* DEC  */
    DECIMAL_P = 373,               /* DECIMAL_P  */
    DECLARE = 374,                 /* DECLARE  */
    DEFAULT = 375,                 /* DEFAULT  */
    DEFAULTS = 376,                /* DEFAULTS  */
    DEFERRABLE = 377,              /* DEFERRABLE  */
    DEFERRED = 378,                /* DEFERRED  */
    DEFINER = 379,                 /* DEFINER  */
    DELETE_P = 380,                /* DELETE_P  */
    DELIMITER = 381,               /* DELIMITER  */
    DELIMITERS = 382,              /* DELIMITERS  */
    DEPENDS = 383,                 /* DEPENDS  */
    DEPTH = 384,                   /* DEPTH  */
    DESC = 385,                    /* DESC  */
    DETACH = 386,                  /* DETACH  */
    DICTIONARY = 387,              /* DICTIONARY  */
    DISABLE_P = 388,               /* DISABLE_P  */
    DISCARD = 389,                 /* DISCARD  */
    DISTINCT = 390,                /* DISTINCT  */
    DO = 391,                      /* DO  */
    DOCUMENT_P = 392,              /* DOCUMENT_P  */
    DOMAIN_P = 393,                /* DOMAIN_P  */
    DOUBLE_P = 394,                /* DOUBLE_P  */
    DROP = 395,                    /* DROP  */
    EACH = 396,                    /* EACH  */
    ELSE = 397,                    /* ELSE  */
    EMPTY_P = 398,                 /* EMPTY_P  */
    ENABLE_P = 399,                /* ENABLE_P  */
    ENCODING = 400,                /* ENCODING  */
    ENCRYPTED = 401,               /* ENCRYPTED  */
    END_P = 402,                   /* END_P  */
    ENFORCED = 403,                /* ENFORCED  */
    ENUM_P = 404,                  /* ENUM_P  */
    ERROR_P = 405,                 /* ERROR_P  */
    ESCAPE = 406,                  /* ESCAPE  */
    EVENT = 407,                   /* EVENT  */
    EXCEPT = 408,                  /* EXCEPT  */
    EXCLUDE = 409,                 /* EXCLUDE  */
    EXCLUDING = 410,               /* EXCLUDING  */
    EXCLUSIVE = 411,               /* EXCLUSIVE  */
    EXECUTE = 412,                 /* EXECUTE  */
    EXISTS = 413,                  /* EXISTS  */
    EXPLAIN = 414,                 /* EXPLAIN  */
    EXPRESSION = 415,              /* EXPRESSION  */
    EXTENSION = 416,               /* EXTENSION  */
    EXTERNAL = 417,                /* EXTERNAL  */
    EXTRACT = 418,                 /* EXTRACT  */
    FALSE_P = 419,                 /* FALSE_P  */
    FAMILY = 420,                  /* FAMILY  */
    FETCH = 421,                   /* FETCH  */
    FILTER = 422,                  /* FILTER  */
    FINALIZE = 423,                /* FINALIZE  */
    FIRST_P = 424,                 /* FIRST_P  */
    FLOAT_P = 425,                 /* FLOAT_P  */
    FOLLOWING = 426,               /* FOLLOWING  */
    FOR = 427,                     /* FOR  */
    FORCE = 428,                   /* FORCE  */
    FOREIGN = 429,                 /* FOREIGN  */
    FORMAT = 430,                  /* FORMAT  */
    FORWARD = 431,                 /* FORWARD  */
    FREEZE = 432,                  /* FREEZE  */
    FROM = 433,                    /* FROM  */
    FULL = 434,                    /* FULL  */
    FUNCTION = 435,                /* FUNCTION  */
    FUNCTIONS = 436,               /* FUNCTIONS  */
    GENERATED = 437,               /* GENERATED  */
    GLOBAL = 438,                  /* GLOBAL  */
    GRANT = 439,                   /* GRANT  */
    GRANTED = 440,                 /* GRANTED  */
    GREATEST = 441,                /* GREATEST  */
    GROUP_P = 442,                 /* GROUP_P  */
    GROUPING = 443,                /* GROUPING  */
    GROUPS = 444,                  /* GROUPS  */
    HANDLER = 445,                 /* HANDLER  */
    HAVING = 446,                  /* HAVING  */
    HEADER_P = 447,                /* HEADER_P  */
    HOLD = 448,                    /* HOLD  */
    HOUR_P = 449,                  /* HOUR_P  */
    IDENTITY_P = 450,              /* IDENTITY_P  */
    IF_P = 451,                    /* IF_P  */
    IGNORE_P = 452,                /* IGNORE_P  */
    ILIKE = 453,                   /* ILIKE  */
    IMMEDIATE = 454,               /* IMMEDIATE  */
    IMMUTABLE = 455,               /* IMMUTABLE  */
    IMPLICIT_P = 456,              /* IMPLICIT_P  */
    IMPORT_P = 457,                /* IMPORT_P  */
    IN_P = 458,                    /* IN_P  */
    INCLUDE = 459,                 /* INCLUDE  */
    INCLUDING = 460,               /* INCLUDING  */
    INCREMENT = 461,               /* INCREMENT  */
    INDENT = 462,                  /* INDENT  */
    INDEX = 463,                   /* INDEX  */
    INDEXES = 464,                 /* INDEXES  */
    INHERIT = 465,                 /* INHERIT  */
    INHERITS = 466,                /* INHERITS  */
    INITIALLY = 467,               /* INITIALLY  */
    INLINE_P = 468,                /* INLINE_P  */
    INNER_P = 469,                 /* INNER_P  */
    INOUT = 470,                   /* INOUT  */
    INPUT_P = 471,                 /* INPUT_P  */
    INSENSITIVE = 472,             /* INSENSITIVE  */
    INSERT = 473,                  /* INSERT  */
    INSTEAD = 474,                 /* INSTEAD  */
    INT_P = 475,                   /* INT_P  */
    INTEGER = 476,                 /* INTEGER  */
    INTERSECT = 477,               /* INTERSECT  */
    INTERVAL = 478,                /* INTERVAL  */
    INTO = 479,                    /* INTO  */
    INVOKER = 480,                 /* INVOKER  */
    IS = 481,                      /* IS  */
    ISNULL = 482,                  /* ISNULL  */
    ISOLATION = 483,               /* ISOLATION  */
    JOIN = 484,                    /* JOIN  */
    JSON = 485,                    /* JSON  */
    JSON_ARRAY = 486,              /* JSON_ARRAY  */
    JSON_ARRAYAGG = 487,           /* JSON_ARRAYAGG  */
    JSON_EXISTS = 488,             /* JSON_EXISTS  */
    JSON_OBJECT = 489,             /* JSON_OBJECT  */
    JSON_OBJECTAGG = 490,          /* JSON_OBJECTAGG  */
    JSON_QUERY = 491,              /* JSON_QUERY  */
    JSON_SCALAR = 492,             /* JSON_SCALAR  */
    JSON_SERIALIZE = 493,          /* JSON_SERIALIZE  */
    JSON_TABLE = 494,              /* JSON_TABLE  */
    JSON_VALUE = 495,              /* JSON_VALUE  */
    KEEP = 496,                    /* KEEP  */
    KEY = 497,                     /* KEY  */
    KEYS = 498,                    /* KEYS  */
    LABEL = 499,                   /* LABEL  */
    LANGUAGE = 500,                /* LANGUAGE  */
    LARGE_P = 501,                 /* LARGE_P  */
    LAST_P = 502,                  /* LAST_P  */
    LATERAL_P = 503,               /* LATERAL_P  */
    LEADING = 504,                 /* LEADING  */
    LEAKPROOF = 505,               /* LEAKPROOF  */
    LEAST = 506,                   /* LEAST  */
    LEFT = 507,                    /* LEFT  */
    LEVEL = 508,                   /* LEVEL  */
    LIKE = 509,                    /* LIKE  */
    LIMIT = 510,                   /* LIMIT  */
    LISTEN = 511,                  /* LISTEN  */
    LOAD = 512,                    /* LOAD  */
    LOCAL = 513,                   /* LOCAL  */
    LOCALTIME = 514,               /* LOCALTIME  */
    LOCALTIMESTAMP = 515,          /* LOCALTIMESTAMP  */
    LOCATION = 516,                /* LOCATION  */
    LOCK_P = 517,                  /* LOCK_P  */
    LOCKED = 518,                  /* LOCKED  */
    LOGGED = 519,                  /* LOGGED  */
    LSN_P = 520,                   /* LSN_P  */
    MAPPING = 521,                 /* MAPPING  */
    MATCH = 522,                   /* MATCH  */
    MATCHED = 523,                 /* MATCHED  */
    MATERIALIZED = 524,            /* MATERIALIZED  */
    MAXVALUE = 525,                /* MAXVALUE  */
    MERGE = 526,                   /* MERGE  */
    MERGE_ACTION = 527,            /* MERGE_ACTION  */
    METHOD = 528,                  /* METHOD  */
    MINUTE_P = 529,                /* MINUTE_P  */
    MINVALUE = 530,                /* MINVALUE  */
    MODE = 531,                    /* MODE  */
    MONTH_P = 532,                 /* MONTH_P  */
    MOVE = 533,                    /* MOVE  */
    NAME_P = 534,                  /* NAME_P  */
    NAMES = 535,                   /* NAMES  */
    NATIONAL = 536,                /* NATIONAL  */
    NATURAL = 537,                 /* NATURAL  */
    NCHAR = 538,                   /* NCHAR  */
    NESTED = 539,                  /* NESTED  */
    NEW = 540,                     /* NEW  */
    NEXT = 541,                    /* NEXT  */
    NFC = 542,                     /* NFC  */
    NFD = 543,                     /* NFD  */
    NFKC = 544,                    /* NFKC  */
    NFKD = 545,                    /* NFKD  */
    NO = 546,                      /* NO  */
    NONE = 547,                    /* NONE  */
    NORMALIZE = 548,               /* NORMALIZE  */
    NORMALIZED = 549,              /* NORMALIZED  */
    NOT = 550,                     /* NOT  */
    NOTHING = 551,                 /* NOTHING  */
    NOTIFY = 552,                  /* NOTIFY  */
    NOTNULL = 553,                 /* NOTNULL  */
    NOWAIT = 554,                  /* NOWAIT  */
    NULL_P = 555,                  /* NULL_P  */
    NULLIF = 556,                  /* NULLIF  */
    NULLS_P = 557,                 /* NULLS_P  */
    NUMERIC = 558,                 /* NUMERIC  */
    OBJECT_P = 559,                /* OBJECT_P  */
    OBJECTS_P = 560,               /* OBJECTS_P  */
    OF = 561,                      /* OF  */
    OFF = 562,                     /* OFF  */
    OFFSET = 563,                  /* OFFSET  */
    OIDS = 564,                    /* OIDS  */
    OLD = 565,                     /* OLD  */
    OMIT = 566,                    /* OMIT  */
    ON = 567,                      /* ON  */
    ONLY = 568,                    /* ONLY  */
    OPERATOR = 569,                /* OPERATOR  */
    OPTION = 570,                  /* OPTION  */
    OPTIONS = 571,                 /* OPTIONS  */
    OR = 572,                      /* OR  */
    ORDER = 573,                   /* ORDER  */
    ORDINALITY = 574,              /* ORDINALITY  */
    OTHERS = 575,                  /* OTHERS  */
    OUT_P = 576,                   /* OUT_P  */
    OUTER_P = 577,                 /* OUTER_P  */
    OVER = 578,                    /* OVER  */
    OVERLAPS = 579,                /* OVERLAPS  */
    OVERLAY = 580,                 /* OVERLAY  */
    OVERRIDING = 581,              /* OVERRIDING  */
    OWNED = 582,                   /* OWNED  */
    OWNER = 583,                   /* OWNER  */
    PARALLEL = 584,                /* PARALLEL  */
    PARAMETER = 585,               /* PARAMETER  */
    PARSER = 586,                  /* PARSER  */
    PARTIAL = 587,                 /* PARTIAL  */
    PARTITION = 588,               /* PARTITION  */
    PASSING = 589,                 /* PASSING  */
    PASSWORD = 590,                /* PASSWORD  */
    PATH = 591,                    /* PATH  */
    PERIOD = 592,                  /* PERIOD  */
    PLACING = 593,                 /* PLACING  */
    PLAN = 594,                    /* PLAN  */
    PLANS = 595,                   /* PLANS  */
    POLICY = 596,                  /* POLICY  */
    POSITION = 597,                /* POSITION  */
    PRECEDING = 598,               /* PRECEDING  */
    PRECISION = 599,               /* PRECISION  */
    PRESERVE = 600,                /* PRESERVE  */
    PREPARE = 601,                 /* PREPARE  */
    PREPARED = 602,                /* PREPARED  */
    PRIMARY = 603,                 /* PRIMARY  */
    PRIOR = 604,                   /* PRIOR  */
    PRIVILEGES = 605,              /* PRIVILEGES  */
    PROCEDURAL = 606,              /* PROCEDURAL  */
    PROCEDURE = 607,               /* PROCEDURE  */
    PROCEDURES = 608,              /* PROCEDURES  */
    PROGRAM = 609,                 /* PROGRAM  */
    PUBLICATION = 610,             /* PUBLICATION  */
    QUOTE = 611,                   /* QUOTE  */
    QUOTES = 612,                  /* QUOTES  */
    RANGE = 613,                   /* RANGE  */
    READ = 614,                    /* READ  */
    REAL = 615,                    /* REAL  */
    REASSIGN = 616,                /* REASSIGN  */
    RECURSIVE = 617,               /* RECURSIVE  */
    REF_P = 618,                   /* REF_P  */
    REFERENCES = 619,              /* REFERENCES  */
    REFERENCING = 620,             /* REFERENCING  */
    REFRESH = 621,                 /* REFRESH  */
    REINDEX = 622,                 /* REINDEX  */
    RELATIVE_P = 623,              /* RELATIVE_P  */
    RELEASE = 624,                 /* RELEASE  */
    RENAME = 625,                  /* RENAME  */
    REPACK = 626,                  /* REPACK  */
    REPEATABLE = 627,              /* REPEATABLE  */
    REPLACE = 628,                 /* REPLACE  */
    REPLICA = 629,                 /* REPLICA  */
    RESET = 630,                   /* RESET  */
    RESPECT_P = 631,               /* RESPECT_P  */
    RESTART = 632,                 /* RESTART  */
    RESTRICT = 633,                /* RESTRICT  */
    RETURN = 634,                  /* RETURN  */
    RETURNING = 635,               /* RETURNING  */
    RETURNS = 636,                 /* RETURNS  */
    REVOKE = 637,                  /* REVOKE  */
    RIGHT = 638,                   /* RIGHT  */
    ROLE = 639,                    /* ROLE  */
    ROLLBACK = 640,                /* ROLLBACK  */
    ROLLUP = 641,                  /* ROLLUP  */
    ROUTINE = 642,                 /* ROUTINE  */
    ROUTINES = 643,                /* ROUTINES  */
    ROW = 644,                     /* ROW  */
    ROWS = 645,                    /* ROWS  */
    RULE = 646,                    /* RULE  */
    SAVEPOINT = 647,               /* SAVEPOINT  */
    SCALAR = 648,                  /* SCALAR  */
    SCHEMA = 649,                  /* SCHEMA  */
    SCHEMAS = 650,                 /* SCHEMAS  */
    SCROLL = 651,                  /* SCROLL  */
    SEARCH = 652,                  /* SEARCH  */
    SECOND_P = 653,                /* SECOND_P  */
    SECURITY = 654,                /* SECURITY  */
    SELECT = 655,                  /* SELECT  */
    SEQUENCE = 656,                /* SEQUENCE  */
    SEQUENCES = 657,               /* SEQUENCES  */
    SERIALIZABLE = 658,            /* SERIALIZABLE  */
    SERVER = 659,                  /* SERVER  */
    SESSION = 660,                 /* SESSION  */
    SESSION_USER = 661,            /* SESSION_USER  */
    SET = 662,                     /* SET  */
    SETS = 663,                    /* SETS  */
    SETOF = 664,                   /* SETOF  */
    SHARE = 665,                   /* SHARE  */
    SHOW = 666,                    /* SHOW  */
    SIMILAR = 667,                 /* SIMILAR  */
    SIMPLE = 668,                  /* SIMPLE  */
    SKIP = 669,                    /* SKIP  */
    SMALLINT = 670,                /* SMALLINT  */
    SNAPSHOT = 671,                /* SNAPSHOT  */
    SOME = 672,                    /* SOME  */
    SOURCE = 673,                  /* SOURCE  */
    SQL_P = 674,                   /* SQL_P  */
    STABLE = 675,                  /* STABLE  */
    STANDALONE_P = 676,            /* STANDALONE_P  */
    START = 677,                   /* START  */
    STATEMENT = 678,               /* STATEMENT  */
    STATISTICS = 679,              /* STATISTICS  */
    STDIN = 680,                   /* STDIN  */
    STDOUT = 681,                  /* STDOUT  */
    STORAGE = 682,                 /* STORAGE  */
    STORED = 683,                  /* STORED  */
    STRICT_P = 684,                /* STRICT_P  */
    STRING_P = 685,                /* STRING_P  */
    STRIP_P = 686,                 /* STRIP_P  */
    SUBSCRIPTION = 687,            /* SUBSCRIPTION  */
    SUBSTRING = 688,               /* SUBSTRING  */
    SUPPORT = 689,                 /* SUPPORT  */
    SYMMETRIC = 690,               /* SYMMETRIC  */
    SYSID = 691,                   /* SYSID  */
    SYSTEM_P = 692,                /* SYSTEM_P  */
    SYSTEM_USER = 693,             /* SYSTEM_USER  */
    TABLE = 694,                   /* TABLE  */
    TABLES = 695,                  /* TABLES  */
    TABLESAMPLE = 696,             /* TABLESAMPLE  */
    TABLESPACE = 697,              /* TABLESPACE  */
    TARGET = 698,                  /* TARGET  */
    TEMP = 699,                    /* TEMP  */
    TEMPLATE = 700,                /* TEMPLATE  */
    TEMPORARY = 701,               /* TEMPORARY  */
    TEXT_P = 702,                  /* TEXT_P  */
    THEN = 703,                    /* THEN  */
    TIES = 704,                    /* TIES  */
    TIME = 705,                    /* TIME  */
    TIMESTAMP = 706,               /* TIMESTAMP  */
    TO = 707,                      /* TO  */
    TRAILING = 708,                /* TRAILING  */
    TRANSACTION = 709,             /* TRANSACTION  */
    TRANSFORM = 710,               /* TRANSFORM  */
    TREAT = 711,                   /* TREAT  */
    TRIGGER = 712,                 /* TRIGGER  */
    TRIM = 713,                    /* TRIM  */
    TRUE_P = 714,                  /* TRUE_P  */
    TRUNCATE = 715,                /* TRUNCATE  */
    TRUSTED = 716,                 /* TRUSTED  */
    TYPE_P = 717,                  /* TYPE_P  */
    TYPES_P = 718,                 /* TYPES_P  */
    UESCAPE = 719,                 /* UESCAPE  */
    UNBOUNDED = 720,               /* UNBOUNDED  */
    UNCONDITIONAL = 721,           /* UNCONDITIONAL  */
    UNCOMMITTED = 722,             /* UNCOMMITTED  */
    UNENCRYPTED = 723,             /* UNENCRYPTED  */
    UNION = 724,                   /* UNION  */
    UNIQUE = 725,                  /* UNIQUE  */
    UNKNOWN = 726,                 /* UNKNOWN  */
    UNLISTEN = 727,                /* UNLISTEN  */
    UNLOGGED = 728,                /* UNLOGGED  */
    UNTIL = 729,                   /* UNTIL  */
    UPDATE = 730,                  /* UPDATE  */
    USER = 731,                    /* USER  */
    USING = 732,                   /* USING  */
    VACUUM = 733,                  /* VACUUM  */
    VALID = 734,                   /* VALID  */
    VALIDATE = 735,                /* VALIDATE  */
    VALIDATOR = 736,               /* VALIDATOR  */
    VALUE_P = 737,                 /* VALUE_P  */
    VALUES = 738,                  /* VALUES  */
    VARCHAR = 739,                 /* VARCHAR  */
    VARIADIC = 740,                /* VARIADIC  */
    VARYING = 741,                 /* VARYING  */
    VERBOSE = 742,                 /* VERBOSE  */
    VERSION_P = 743,               /* VERSION_P  */
    VIEW = 744,                    /* VIEW  */
    VIEWS = 745,                   /* VIEWS  */
    VIRTUAL = 746,                 /* VIRTUAL  */
    VOLATILE = 747,                /* VOLATILE  */
    WAIT = 748,                    /* WAIT  */
    WHEN = 749,                    /* WHEN  */
    WHERE = 750,                   /* WHERE  */
    WHITESPACE_P = 751,            /* WHITESPACE_P  */
    WINDOW = 752,                  /* WINDOW  */
    WITH = 753,                    /* WITH  */
    WITHIN = 754,                  /* WITHIN  */
    WITHOUT = 755,                 /* WITHOUT  */
    WORK = 756,                    /* WORK  */
    WRAPPER = 757,                 /* WRAPPER  */
    WRITE = 758,                   /* WRITE  */
    XML_P = 759,                   /* XML_P  */
    XMLATTRIBUTES = 760,           /* XMLATTRIBUTES  */
    XMLCONCAT = 761,               /* XMLCONCAT  */
    XMLELEMENT = 762,              /* XMLELEMENT  */
    XMLEXISTS = 763,               /* XMLEXISTS  */
    XMLFOREST = 764,               /* XMLFOREST  */
    XMLNAMESPACES = 765,           /* XMLNAMESPACES  */
    XMLPARSE = 766,                /* XMLPARSE  */
    XMLPI = 767,                   /* XMLPI  */
    XMLROOT = 768,                 /* XMLROOT  */
    XMLSERIALIZE = 769,            /* XMLSERIALIZE  */
    XMLTABLE = 770,                /* XMLTABLE  */
    YEAR_P = 771,                  /* YEAR_P  */
    YES_P = 772,                   /* YES_P  */
    ZONE = 773,                    /* ZONE  */
    FORMAT_LA = 774,               /* FORMAT_LA  */
    NOT_LA = 775,                  /* NOT_LA  */
    NULLS_LA = 776,                /* NULLS_LA  */
    WITH_LA = 777,                 /* WITH_LA  */
    WITHOUT_LA = 778,              /* WITHOUT_LA  */
    MODE_TYPE_NAME = 779,          /* MODE_TYPE_NAME  */
    MODE_PLPGSQL_EXPR = 780,       /* MODE_PLPGSQL_EXPR  */
    MODE_PLPGSQL_ASSIGN1 = 781,    /* MODE_PLPGSQL_ASSIGN1  */
    MODE_PLPGSQL_ASSIGN2 = 782,    /* MODE_PLPGSQL_ASSIGN2  */
    MODE_PLPGSQL_ASSIGN3 = 783,    /* MODE_PLPGSQL_ASSIGN3  */
    UMINUS = 784                   /* UMINUS  */
  };
  typedef enum yytokentype yytoken_kind_t;
#endif

/* Value type.  */
#if ! defined YYSTYPE && ! defined YYSTYPE_IS_DECLARED
union YYSTYPE
{
#line 224 "gram.y"

	core_YYSTYPE core_yystype;
	/* these fields must match core_YYSTYPE: */
	int			ival;
	char	   *str;
	const char *keyword;

	char		chr;
	bool		boolean;
	JoinType	jtype;
	DropBehavior dbehavior;
	OnCommitAction oncommit;
	List	   *list;
	Node	   *node;
	ObjectType	objtype;
	TypeName   *typnam;
	FunctionParameter *fun_param;
	FunctionParameterMode fun_param_mode;
	ObjectWithArgs *objwithargs;
	DefElem	   *defelt;
	SortBy	   *sortby;
	WindowDef  *windef;
	JoinExpr   *jexpr;
	IndexElem  *ielem;
	StatsElem  *selem;
	Alias	   *alias;
	RangeVar   *range;
	IntoClause *into;
	WithClause *with;
	InferClause	*infer;
	OnConflictClause *onconflict;
	A_Indices  *aind;
	ResTarget  *target;
	struct PrivTarget *privtarget;
	AccessPriv *accesspriv;
	struct ImportQual *importqual;
	InsertStmt *istmt;
	VariableSetStmt *vsetstmt;
	PartitionElem *partelem;
	PartitionSpec *partspec;
	PartitionBoundSpec *partboundspec;
	RoleSpec   *rolespec;
	PublicationObjSpec *publicationobjectspec;
	PublicationAllObjSpec *publicationallobjectspec;
	struct SelectLimit *selectlimit;
	SetQuantifier setquantifier;
	struct GroupClause *groupclause;
	MergeMatchKind mergematch;
	MergeWhenClause *mergewhen;
	struct KeyActions *keyactions;
	struct KeyAction *keyaction;
	ReturningClause *retclause;
	ReturningOptionKind retoptionkind;

#line 648 "gram.h"

};
typedef union YYSTYPE YYSTYPE;
# define YYSTYPE_IS_TRIVIAL 1
# define YYSTYPE_IS_DECLARED 1
#endif

/* Location type.  */
#if ! defined YYLTYPE && ! defined YYLTYPE_IS_DECLARED
typedef struct YYLTYPE YYLTYPE;
struct YYLTYPE
{
  int first_line;
  int first_column;
  int last_line;
  int last_column;
};
# define YYLTYPE_IS_DECLARED 1
# define YYLTYPE_IS_TRIVIAL 1
#endif




int base_yyparse (core_yyscan_t yyscanner);


#endif /* !YY_BASE_YY_GRAM_H_INCLUDED  */
