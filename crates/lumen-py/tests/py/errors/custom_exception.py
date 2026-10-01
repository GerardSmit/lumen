class ValidationError(Exception):
    pass


def validate(x):
    if x < 0:
        raise ValidationError("negative value not allowed")
    return x


print("validating")
print(validate(5))
print(validate(-1))
print("unreachable")
