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
    ILIKE = 452,                   /* ILIKE  */
    IMMEDIATE = 453,               /* IMMEDIATE  */
    IMMUTABLE = 454,               /* IMMUTABLE  */
    IMPLICIT_P = 455,              /* IMPLICIT_P  */
    IMPORT_P = 456,                /* IMPORT_P  */
    IN_P = 457,                    /* IN_P  */
    INCLUDE = 458,                 /* INCLUDE  */
    INCLUDING = 459,               /* INCLUDING  */
    INCREMENT = 460,               /* INCREMENT  */
    INDENT = 461,                  /* INDENT  */
    INDEX = 462,                   /* INDEX  */
    INDEXES = 463,                 /* INDEXES  */
    INHERIT = 464,                 /* INHERIT  */
    INHERITS = 465,                /* INHERITS  */
    INITIALLY = 466,               /* INITIALLY  */
    INLINE_P = 467,                /* INLINE_P  */
    INNER_P = 468,                 /* INNER_P  */
    INOUT = 469,                   /* INOUT  */
    INPUT_P = 470,                 /* INPUT_P  */
    INSENSITIVE = 471,             /* INSENSITIVE  */
    INSERT = 472,                  /* INSERT  */
    INSTEAD = 473,                 /* INSTEAD  */
    INT_P = 474,                   /* INT_P  */
    INTEGER = 475,                 /* INTEGER  */
    INTERSECT = 476,               /* INTERSECT  */
    INTERVAL = 477,                /* INTERVAL  */
    INTO = 478,                    /* INTO  */
    INVOKER = 479,                 /* INVOKER  */
    IS = 480,                      /* IS  */
    ISNULL = 481,                  /* ISNULL  */
    ISOLATION = 482,               /* ISOLATION  */
    JOIN = 483,                    /* JOIN  */
    JSON = 484,                    /* JSON  */
    JSON_ARRAY = 485,              /* JSON_ARRAY  */
    JSON_ARRAYAGG = 486,           /* JSON_ARRAYAGG  */
    JSON_EXISTS = 487,             /* JSON_EXISTS  */
    JSON_OBJECT = 488,             /* JSON_OBJECT  */
    JSON_OBJECTAGG = 489,          /* JSON_OBJECTAGG  */
    JSON_QUERY = 490,              /* JSON_QUERY  */
    JSON_SCALAR = 491,             /* JSON_SCALAR  */
    JSON_SERIALIZE = 492,          /* JSON_SERIALIZE  */
    JSON_TABLE = 493,              /* JSON_TABLE  */
    JSON_VALUE = 494,              /* JSON_VALUE  */
    KEEP = 495,                    /* KEEP  */
    KEY = 496,                     /* KEY  */
    KEYS = 497,                    /* KEYS  */
    LABEL = 498,                   /* LABEL  */
    LANGUAGE = 499,                /* LANGUAGE  */
    LARGE_P = 500,                 /* LARGE_P  */
    LAST_P = 501,                  /* LAST_P  */
    LATERAL_P = 502,               /* LATERAL_P  */
    LEADING = 503,                 /* LEADING  */
    LEAKPROOF = 504,               /* LEAKPROOF  */
    LEAST = 505,                   /* LEAST  */
    LEFT = 506,                    /* LEFT  */
    LEVEL = 507,                   /* LEVEL  */
    LIKE = 508,                    /* LIKE  */
    LIMIT = 509,                   /* LIMIT  */
    LISTEN = 510,                  /* LISTEN  */
    LOAD = 511,                    /* LOAD  */
    LOCAL = 512,                   /* LOCAL  */
    LOCALTIME = 513,               /* LOCALTIME  */
    LOCALTIMESTAMP = 514,          /* LOCALTIMESTAMP  */
    LOCATION = 515,                /* LOCATION  */
    LOCK_P = 516,                  /* LOCK_P  */
    LOCKED = 517,                  /* LOCKED  */
    LOGGED = 518,                  /* LOGGED  */
    MAPPING = 519,                 /* MAPPING  */
    MATCH = 520,                   /* MATCH  */
    MATCHED = 521,                 /* MATCHED  */
    MATERIALIZED = 522,            /* MATERIALIZED  */
    MAXVALUE = 523,                /* MAXVALUE  */
    MERGE = 524,                   /* MERGE  */
    MERGE_ACTION = 525,            /* MERGE_ACTION  */
    METHOD = 526,                  /* METHOD  */
    MINUTE_P = 527,                /* MINUTE_P  */
    MINVALUE = 528,                /* MINVALUE  */
    MODE = 529,                    /* MODE  */
    MONTH_P = 530,                 /* MONTH_P  */
    MOVE = 531,                    /* MOVE  */
    NAME_P = 532,                  /* NAME_P  */
    NAMES = 533,                   /* NAMES  */
    NATIONAL = 534,                /* NATIONAL  */
    NATURAL = 535,                 /* NATURAL  */
    NCHAR = 536,                   /* NCHAR  */
    NESTED = 537,                  /* NESTED  */
    NEW = 538,                     /* NEW  */
    NEXT = 539,                    /* NEXT  */
    NFC = 540,                     /* NFC  */
    NFD = 541,                     /* NFD  */
    NFKC = 542,                    /* NFKC  */
    NFKD = 543,                    /* NFKD  */
    NO = 544,                      /* NO  */
    NONE = 545,                    /* NONE  */
    NORMALIZE = 546,               /* NORMALIZE  */
    NORMALIZED = 547,              /* NORMALIZED  */
    NOT = 548,                     /* NOT  */
    NOTHING = 549,                 /* NOTHING  */
    NOTIFY = 550,                  /* NOTIFY  */
    NOTNULL = 551,                 /* NOTNULL  */
    NOWAIT = 552,                  /* NOWAIT  */
    NULL_P = 553,                  /* NULL_P  */
    NULLIF = 554,                  /* NULLIF  */
    NULLS_P = 555,                 /* NULLS_P  */
    NUMERIC = 556,                 /* NUMERIC  */
    OBJECT_P = 557,                /* OBJECT_P  */
    OBJECTS_P = 558,               /* OBJECTS_P  */
    OF = 559,                      /* OF  */
    OFF = 560,                     /* OFF  */
    OFFSET = 561,                  /* OFFSET  */
    OIDS = 562,                    /* OIDS  */
    OLD = 563,                     /* OLD  */
    OMIT = 564,                    /* OMIT  */
    ON = 565,                      /* ON  */
    ONLY = 566,                    /* ONLY  */
    OPERATOR = 567,                /* OPERATOR  */
    OPTION = 568,                  /* OPTION  */
    OPTIONS = 569,                 /* OPTIONS  */
    OR = 570,                      /* OR  */
    ORDER = 571,                   /* ORDER  */
    ORDINALITY = 572,              /* ORDINALITY  */
    OTHERS = 573,                  /* OTHERS  */
    OUT_P = 574,                   /* OUT_P  */
    OUTER_P = 575,                 /* OUTER_P  */
    OVER = 576,                    /* OVER  */
    OVERLAPS = 577,                /* OVERLAPS  */
    OVERLAY = 578,                 /* OVERLAY  */
    OVERRIDING = 579,              /* OVERRIDING  */
    OWNED = 580,                   /* OWNED  */
    OWNER = 581,                   /* OWNER  */
    PARALLEL = 582,                /* PARALLEL  */
    PARAMETER = 583,               /* PARAMETER  */
    PARSER = 584,                  /* PARSER  */
    PARTIAL = 585,                 /* PARTIAL  */
    PARTITION = 586,               /* PARTITION  */
    PASSING = 587,                 /* PASSING  */
    PASSWORD = 588,                /* PASSWORD  */
    PATH = 589,                    /* PATH  */
    PERIOD = 590,                  /* PERIOD  */
    PLACING = 591,                 /* PLACING  */
    PLAN = 592,                    /* PLAN  */
    PLANS = 593,                   /* PLANS  */
    POLICY = 594,                  /* POLICY  */
    POSITION = 595,                /* POSITION  */
    PRECEDING = 596,               /* PRECEDING  */
    PRECISION = 597,               /* PRECISION  */
    PRESERVE = 598,                /* PRESERVE  */
    PREPARE = 599,                 /* PREPARE  */
    PREPARED = 600,                /* PREPARED  */
    PRIMARY = 601,                 /* PRIMARY  */
    PRIOR = 602,                   /* PRIOR  */
    PRIVILEGES = 603,              /* PRIVILEGES  */
    PROCEDURAL = 604,              /* PROCEDURAL  */
    PROCEDURE = 605,               /* PROCEDURE  */
    PROCEDURES = 606,              /* PROCEDURES  */
    PROGRAM = 607,                 /* PROGRAM  */
    PUBLICATION = 608,             /* PUBLICATION  */
    QUOTE = 609,                   /* QUOTE  */
    QUOTES = 610,                  /* QUOTES  */
    RANGE = 611,                   /* RANGE  */
    READ = 612,                    /* READ  */
    REAL = 613,                    /* REAL  */
    REASSIGN = 614,                /* REASSIGN  */
    RECURSIVE = 615,               /* RECURSIVE  */
    REF_P = 616,                   /* REF_P  */
    REFERENCES = 617,              /* REFERENCES  */
    REFERENCING = 618,             /* REFERENCING  */
    REFRESH = 619,                 /* REFRESH  */
    REINDEX = 620,                 /* REINDEX  */
    RELATIVE_P = 621,              /* RELATIVE_P  */
    RELEASE = 622,                 /* RELEASE  */
    RENAME = 623,                  /* RENAME  */
    REPEATABLE = 624,              /* REPEATABLE  */
    REPLACE = 625,                 /* REPLACE  */
    REPLICA = 626,                 /* REPLICA  */
    RESET = 627,                   /* RESET  */
    RESTART = 628,                 /* RESTART  */
    RESTRICT = 629,                /* RESTRICT  */
    RETURN = 630,                  /* RETURN  */
    RETURNING = 631,               /* RETURNING  */
    RETURNS = 632,                 /* RETURNS  */
    REVOKE = 633,                  /* REVOKE  */
    RIGHT = 634,                   /* RIGHT  */
    ROLE = 635,                    /* ROLE  */
    ROLLBACK = 636,                /* ROLLBACK  */
    ROLLUP = 637,                  /* ROLLUP  */
    ROUTINE = 638,                 /* ROUTINE  */
    ROUTINES = 639,                /* ROUTINES  */
    ROW = 640,                     /* ROW  */
    ROWS = 641,                    /* ROWS  */
    RULE = 642,                    /* RULE  */
    SAVEPOINT = 643,               /* SAVEPOINT  */
    SCALAR = 644,                  /* SCALAR  */
    SCHEMA = 645,                  /* SCHEMA  */
    SCHEMAS = 646,                 /* SCHEMAS  */
    SCROLL = 647,                  /* SCROLL  */
    SEARCH = 648,                  /* SEARCH  */
    SECOND_P = 649,                /* SECOND_P  */
    SECURITY = 650,                /* SECURITY  */
    SELECT = 651,                  /* SELECT  */
    SEQUENCE = 652,                /* SEQUENCE  */
    SEQUENCES = 653,               /* SEQUENCES  */
    SERIALIZABLE = 654,            /* SERIALIZABLE  */
    SERVER = 655,                  /* SERVER  */
    SESSION = 656,                 /* SESSION  */
    SESSION_USER = 657,            /* SESSION_USER  */
    SET = 658,                     /* SET  */
    SETS = 659,                    /* SETS  */
    SETOF = 660,                   /* SETOF  */
    SHARE = 661,                   /* SHARE  */
    SHOW = 662,                    /* SHOW  */
    SIMILAR = 663,                 /* SIMILAR  */
    SIMPLE = 664,                  /* SIMPLE  */
    SKIP = 665,                    /* SKIP  */
    SMALLINT = 666,                /* SMALLINT  */
    SNAPSHOT = 667,                /* SNAPSHOT  */
    SOME = 668,                    /* SOME  */
    SOURCE = 669,                  /* SOURCE  */
    SQL_P = 670,                   /* SQL_P  */
    STABLE = 671,                  /* STABLE  */
    STANDALONE_P = 672,            /* STANDALONE_P  */
    START = 673,                   /* START  */
    STATEMENT = 674,               /* STATEMENT  */
    STATISTICS = 675,              /* STATISTICS  */
    STDIN = 676,                   /* STDIN  */
    STDOUT = 677,                  /* STDOUT  */
    STORAGE = 678,                 /* STORAGE  */
    STORED = 679,                  /* STORED  */
    STRICT_P = 680,                /* STRICT_P  */
    STRING_P = 681,                /* STRING_P  */
    STRIP_P = 682,                 /* STRIP_P  */
    SUBSCRIPTION = 683,            /* SUBSCRIPTION  */
    SUBSTRING = 684,               /* SUBSTRING  */
    SUPPORT = 685,                 /* SUPPORT  */
    SYMMETRIC = 686,               /* SYMMETRIC  */
    SYSID = 687,                   /* SYSID  */
    SYSTEM_P = 688,                /* SYSTEM_P  */
    SYSTEM_USER = 689,             /* SYSTEM_USER  */
    TABLE = 690,                   /* TABLE  */
    TABLES = 691,                  /* TABLES  */
    TABLESAMPLE = 692,             /* TABLESAMPLE  */
    TABLESPACE = 693,              /* TABLESPACE  */
    TARGET = 694,                  /* TARGET  */
    TEMP = 695,                    /* TEMP  */
    TEMPLATE = 696,                /* TEMPLATE  */
    TEMPORARY = 697,               /* TEMPORARY  */
    TEXT_P = 698,                  /* TEXT_P  */
    THEN = 699,                    /* THEN  */
    TIES = 700,                    /* TIES  */
    TIME = 701,                    /* TIME  */
    TIMESTAMP = 702,               /* TIMESTAMP  */
    TO = 703,                      /* TO  */
    TRAILING = 704,                /* TRAILING  */
    TRANSACTION = 705,             /* TRANSACTION  */
    TRANSFORM = 706,               /* TRANSFORM  */
    TREAT = 707,                   /* TREAT  */
    TRIGGER = 708,                 /* TRIGGER  */
    TRIM = 709,                    /* TRIM  */
    TRUE_P = 710,                  /* TRUE_P  */
    TRUNCATE = 711,                /* TRUNCATE  */
    TRUSTED = 712,                 /* TRUSTED  */
    TYPE_P = 713,                  /* TYPE_P  */
    TYPES_P = 714,                 /* TYPES_P  */
    UESCAPE = 715,                 /* UESCAPE  */
    UNBOUNDED = 716,               /* UNBOUNDED  */
    UNCONDITIONAL = 717,           /* UNCONDITIONAL  */
    UNCOMMITTED = 718,             /* UNCOMMITTED  */
    UNENCRYPTED = 719,             /* UNENCRYPTED  */
    UNION = 720,                   /* UNION  */
    UNIQUE = 721,                  /* UNIQUE  */
    UNKNOWN = 722,                 /* UNKNOWN  */
    UNLISTEN = 723,                /* UNLISTEN  */
    UNLOGGED = 724,                /* UNLOGGED  */
    UNTIL = 725,                   /* UNTIL  */
    UPDATE = 726,                  /* UPDATE  */
    USER = 727,                    /* USER  */
    USING = 728,                   /* USING  */
    VACUUM = 729,                  /* VACUUM  */
    VALID = 730,                   /* VALID  */
    VALIDATE = 731,                /* VALIDATE  */
    VALIDATOR = 732,               /* VALIDATOR  */
    VALUE_P = 733,                 /* VALUE_P  */
    VALUES = 734,                  /* VALUES  */
    VARCHAR = 735,                 /* VARCHAR  */
    VARIADIC = 736,                /* VARIADIC  */
    VARYING = 737,                 /* VARYING  */
    VERBOSE = 738,                 /* VERBOSE  */
    VERSION_P = 739,               /* VERSION_P  */
    VIEW = 740,                    /* VIEW  */
    VIEWS = 741,                   /* VIEWS  */
    VIRTUAL = 742,                 /* VIRTUAL  */
    VOLATILE = 743,                /* VOLATILE  */
    WHEN = 744,                    /* WHEN  */
    WHERE = 745,                   /* WHERE  */
    WHITESPACE_P = 746,            /* WHITESPACE_P  */
    WINDOW = 747,                  /* WINDOW  */
    WITH = 748,                    /* WITH  */
    WITHIN = 749,                  /* WITHIN  */
    WITHOUT = 750,                 /* WITHOUT  */
    WORK = 751,                    /* WORK  */
    WRAPPER = 752,                 /* WRAPPER  */
    WRITE = 753,                   /* WRITE  */
    XML_P = 754,                   /* XML_P  */
    XMLATTRIBUTES = 755,           /* XMLATTRIBUTES  */
    XMLCONCAT = 756,               /* XMLCONCAT  */
    XMLELEMENT = 757,              /* XMLELEMENT  */
    XMLEXISTS = 758,               /* XMLEXISTS  */
    XMLFOREST = 759,               /* XMLFOREST  */
    XMLNAMESPACES = 760,           /* XMLNAMESPACES  */
    XMLPARSE = 761,                /* XMLPARSE  */
    XMLPI = 762,                   /* XMLPI  */
    XMLROOT = 763,                 /* XMLROOT  */
    XMLSERIALIZE = 764,            /* XMLSERIALIZE  */
    XMLTABLE = 765,                /* XMLTABLE  */
    YEAR_P = 766,                  /* YEAR_P  */
    YES_P = 767,                   /* YES_P  */
    ZONE = 768,                    /* ZONE  */
    FORMAT_LA = 769,               /* FORMAT_LA  */
    NOT_LA = 770,                  /* NOT_LA  */
    NULLS_LA = 771,                /* NULLS_LA  */
    WITH_LA = 772,                 /* WITH_LA  */
    WITHOUT_LA = 773,              /* WITHOUT_LA  */
    MODE_TYPE_NAME = 774,          /* MODE_TYPE_NAME  */
    MODE_PLPGSQL_EXPR = 775,       /* MODE_PLPGSQL_EXPR  */
    MODE_PLPGSQL_ASSIGN1 = 776,    /* MODE_PLPGSQL_ASSIGN1  */
    MODE_PLPGSQL_ASSIGN2 = 777,    /* MODE_PLPGSQL_ASSIGN2  */
    MODE_PLPGSQL_ASSIGN3 = 778,    /* MODE_PLPGSQL_ASSIGN3  */
    UMINUS = 779                   /* UMINUS  */
  };
  typedef enum yytokentype yytoken_kind_t;
#endif

/* Value type.  */
#if ! defined YYSTYPE && ! defined YYSTYPE_IS_DECLARED
union YYSTYPE
{
#line 219 "gram.y"

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
	struct SelectLimit *selectlimit;
	SetQuantifier setquantifier;
	struct GroupClause *groupclause;
	MergeMatchKind mergematch;
	MergeWhenClause *mergewhen;
	struct KeyActions *keyactions;
	struct KeyAction *keyaction;
	ReturningClause *retclause;
	ReturningOptionKind retoptionkind;

#line 642 "gram.h"

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
