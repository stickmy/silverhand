// A destructuring default lives on the binding, so exhaustive-deps does not
// see it as the expression that constructs the dependency.
const hooks = new Set(['useMemo', 'useCallback', 'useEffect', 'useLayoutEffect']);

function isReactHook(callee, scope) {
  const member = callee.type === 'MemberExpression' && !callee.computed;
  const name = member ? callee.object.name : callee.name;
  if (!name) return false;
  while (scope && !scope.set.has(name)) scope = scope.upper;
  const def = scope?.set.get(name)?.defs[0];
  if (def?.type !== 'ImportBinding' || def.parent.source.value !== 'react') return false;
  if (member) return def.node.type !== 'ImportSpecifier' && hooks.has(callee.property.name);
  return def.node.type === 'ImportSpecifier' && hooks.has(def.node.imported.name);
}

function literalDefault(identifier) {
  const assignment = identifier?.parent;
  if (assignment?.type !== 'AssignmentPattern' || assignment.left !== identifier) return;
  let value = assignment.right;
  while (value.type === 'TSAsExpression' || value.type === 'TSSatisfiesExpression') {
    value = value.expression;
  }
  return value.type === 'ArrayExpression' || value.type === 'ObjectExpression';
}

export default {
  meta: {
    type: 'problem',
    schema: [],
    messages: {
      unstable:
        "'{{name}}' defaults to a new reference on each render without a value. Use a stable fallback or construct it inside the hook.",
    },
  },
  create(context) {
    return {
      CallExpression(node) {
        const deps = node.arguments[1];
        if (deps?.type !== 'ArrayExpression') return;
        const scope = context.sourceCode.getScope(node);
        if (scope.variableScope.type !== 'function' || !isReactHook(node.callee, scope)) return;
        for (const dep of deps.elements) {
          if (dep?.type !== 'Identifier') continue;
          const variable = scope.references.find((ref) => ref.identifier === dep)?.resolved;
          if (variable?.scope.variableScope !== scope.variableScope) continue;
          if (variable.identifiers.some(literalDefault)) {
            context.report({ node: dep, messageId: 'unstable', data: { name: dep.name } });
          }
        }
      },
    };
  },
};
