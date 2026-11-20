/*
 * parse_defaults.c — the expression transformers PostgreSQL runs on a
 * parameter DEFAULT, left as the identity.
 *
 * Compiling a function's signature needs the parameter and result types;
 * PostgreSQL also transforms each DEFAULT expression to check it.  That
 * transformation is the whole expression parser (parse_expr.c plus the
 * function and operator resolution behind it: parse_func.c, parse_oper.c,
 * parse_clause.c, …), which this crate does not carry — typedpg's analyzer
 * validates default expressions itself in its DDL layer, and PostgreSQL
 * re-checks them when a function is called.
 *
 * libpg_query's extraction did exactly this: transformExpr returned the raw
 * tree, the coercion and collation passes were no-ops, and the check for
 * Vars in a default was left to return false (the real walker in
 * optimizer/util/var.c only understands transformed trees).  Keeping the same
 * behavior means the signature checks of interpret_function_parameter_list
 * (types, modes, VARIADIC, defaults ordering) still run.
 */
#include "postgres.h"

#include "parser/parse_coerce.h"
#include "parser/parse_collate.h"
#include "parser/parse_expr.h"
#include "optimizer/optimizer.h"

Node *
transformExpr(ParseState *pstate, Node *expr, ParseExprKind exprKind)
{
	return expr;
}

Node *
coerce_to_specific_type(ParseState *pstate, Node *node,
						Oid targetTypeId, const char *constructName)
{
	return node;
}

void
assign_expr_collations(ParseState *pstate, Node *expr)
{
}

bool
contain_var_clause(Node *node)
{
	return false;
}
