// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::borrow::Borrow;
use std::convert::{From, TryInto};
use std::fmt;
use std::{any::Any, iter::FromIterator};

use super::BooleanBufferBuilder;
use super::{
    array::print_long_array, raw_pointer::RawPtrBox, Array, ArrayData, FixedSizeListArray,
};
pub use crate::array::DecimalIter;
use crate::buffer::Buffer;
use crate::datatypes::DataType;
use crate::datatypes::{
    validate_decimal_precision, DECIMAL_DEFAULT_SCALE, DECIMAL_MAX_PRECISION,
    DECIMAL_MAX_SCALE,
};
use crate::error::{ArrowError, Result};
use crate::util::decimal::{BasicDecimal, Decimal128};

/// `DecimalArray` stores fixed width decimal numbers,
/// with a fixed precision and scale.
///
/// # Examples
///
/// ```
///    use arrow::array::{Array, DecimalArray};
///    use arrow::datatypes::DataType;
///
///    // Create a DecimalArray with the default precision and scale
///    let decimal_array: DecimalArray = vec![
///       Some(8_887_000_000),
///       None,
///       Some(-8_887_000_000),
///     ]
///     .into_iter().collect();
///
///    // set precision and scale so values are interpreted
///    // as `8887.000000`, `Null`, and `-8887.000000`
///    let decimal_array = decimal_array
///     .with_precision_and_scale(23, 6)
///     .unwrap();
///
///    assert_eq!(&DataType::Decimal(23, 6), decimal_array.data_type());
///    assert_eq!(8_887_000_000_i128, decimal_array.value(0).as_i128());
///    assert_eq!("8887.000000", decimal_array.value_as_string(0));
///    assert_eq!(3, decimal_array.len());
///    assert_eq!(1, decimal_array.null_count());
///    assert_eq!(32, decimal_array.value_offset(2));
///    assert_eq!(16, decimal_array.value_length());
///    assert_eq!(23, decimal_array.precision());
///    assert_eq!(6, decimal_array.scale());
/// ```
///
pub struct DecimalArray {
    data: ArrayData,
    value_data: RawPtrBox<u8>,
    precision: usize,
    scale: usize,
}

impl DecimalArray {
    const VALUE_LENGTH: i32 = 16;

    /// Returns the element at index `i`.
    pub fn value(&self, i: usize) -> Decimal128 {
        assert!(i < self.data.len(), "DecimalArray out of bounds access");
        let offset = i + self.data.offset();
        let raw_val = unsafe {
            let pos = self.value_offset_at(offset);
            std::slice::from_raw_parts(
                self.value_data.as_ptr().offset(pos as isize),
                Self::VALUE_LENGTH as usize,
            )
        };
        let as_array = raw_val.try_into().unwrap();
        Decimal128::new_from_i128(
            self.precision,
            self.scale,
            i128::from_le_bytes(as_array),
        )
    }

    /// Returns the offset for the element at index `i`.
    ///
    /// Note this doesn't do any bound checking, for performance reason.
    #[inline]
    pub fn value_offset(&self, i: usize) -> i32 {
        self.value_offset_at(self.data.offset() + i)
    }

    /// Returns the length for an element.
    ///
    /// All elements have the same length as the array is a fixed size.
    #[inline]
    pub const fn value_length(&self) -> i32 {
        Self::VALUE_LENGTH
    }

    /// Returns a clone of the value data buffer
    pub fn value_data(&self) -> Buffer {
        self.data.buffers()[0].clone()
    }

    #[inline]
    fn value_offset_at(&self, i: usize) -> i32 {
        Self::VALUE_LENGTH * i as i32
    }

    #[inline]
    pub fn value_as_string(&self, row: usize) -> String {
        self.value(row).to_string()
    }

    pub fn from_fixed_size_list_array(
        v: FixedSizeListArray,
        precision: usize,
        scale: usize,
    ) -> Self {
        let child_data = &v.data_ref().child_data()[0];
        assert_eq!(
            child_data.child_data().len(),
            0,
            "DecimalArray can only be created from list array of u8 values \
             (i.e. FixedSizeList<PrimitiveArray<u8>>)."
        );
        assert_eq!(
            child_data.data_type(),
            &DataType::UInt8,
            "DecimalArray can only be created from FixedSizeList<u8> arrays, mismatched data types."
        );

        let list_offset = v.offset();
        let child_offset = child_data.offset();
        let builder = ArrayData::builder(DataType::Decimal(precision, scale))
            .len(v.len())
            .add_buffer(child_data.buffers()[0].slice(child_offset))
            .null_bit_buffer(v.data_ref().null_buffer().cloned())
            .offset(list_offset);

        let array_data = unsafe { builder.build_unchecked() };
        Self::from(array_data)
    }

    /// Creates a [DecimalArray] with default precision and scale,
    /// based on an iterator of `i128` values without nulls
    pub fn from_iter_values<I: IntoIterator<Item = i128>>(iter: I) -> Self {
        let val_buf: Buffer = iter.into_iter().collect();
        let data = unsafe {
            ArrayData::new_unchecked(
                Self::default_type(),
                val_buf.len() / std::mem::size_of::<i128>(),
                None,
                None,
                0,
                vec![val_buf],
                vec![],
            )
        };
        DecimalArray::from(data)
    }

    /// Return the precision (total digits) that can be stored by this array
    pub fn precision(&self) -> usize {
        self.precision
    }

    /// Return the scale (digits after the decimal) that can be stored by this array
    pub fn scale(&self) -> usize {
        self.scale
    }

    /// Returns a DecimalArray with the same data as self, with the
    /// specified precision.
    ///
    /// Returns an Error if:
    /// 1. `precision` is larger than [`DECIMAL_MAX_PRECISION`]
    /// 2. `scale` is larger than [`DECIMAL_MAX_SCALE`];
    /// 3. `scale` is > `precision`
    pub fn with_precision_and_scale(
        mut self,
        precision: usize,
        scale: usize,
    ) -> Result<Self> {
        if precision > DECIMAL_MAX_PRECISION {
            return Err(ArrowError::InvalidArgumentError(format!(
                "precision {} is greater than max {}",
                precision, DECIMAL_MAX_PRECISION
            )));
        }
        if scale > DECIMAL_MAX_SCALE {
            return Err(ArrowError::InvalidArgumentError(format!(
                "scale {} is greater than max {}",
                scale, DECIMAL_MAX_SCALE
            )));
        }
        if scale > precision {
            return Err(ArrowError::InvalidArgumentError(format!(
                "scale {} is greater than precision {}",
                scale, precision
            )));
        }

        // Ensure that all values are within the requested
        // precision. For performance, only check if the precision is
        // decreased
        if precision < self.precision {
            for v in self.iter().flatten() {
                validate_decimal_precision(v, precision)?;
            }
        }

        assert_eq!(
            self.data.data_type(),
            &DataType::Decimal(self.precision, self.scale)
        );

        // safety: self.data is valid DataType::Decimal as checked above
        let new_data_type = DataType::Decimal(precision, scale);
        self.precision = precision;
        self.scale = scale;
        self.data = self.data.with_data_type(new_data_type);
        Ok(self)
    }

    /// The default precision and scale used when not specified.
    pub fn default_type() -> DataType {
        // Keep maximum precision
        DataType::Decimal(DECIMAL_MAX_PRECISION, DECIMAL_DEFAULT_SCALE)
    }
}

impl From<ArrayData> for DecimalArray {
    fn from(data: ArrayData) -> Self {
        assert_eq!(
            data.buffers().len(),
            1,
            "DecimalArray data should contain 1 buffer only (values)"
        );
        let values = data.buffers()[0].as_ptr();
        let (precision, scale) = match data.data_type() {
            DataType::Decimal(precision, scale) => (*precision, *scale),
            _ => panic!("Expected data type to be Decimal"),
        };
        Self {
            data,
            value_data: unsafe { RawPtrBox::new(values) },
            precision,
            scale,
        }
    }
}

impl From<DecimalArray> for ArrayData {
    fn from(array: DecimalArray) -> Self {
        array.data
    }
}

impl<'a> IntoIterator for &'a DecimalArray {
    type Item = Option<i128>;
    type IntoIter = DecimalIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        DecimalIter::<'a>::new(self)
    }
}

impl<'a> DecimalArray {
    /// constructs a new iterator
    pub fn iter(&'a self) -> DecimalIter<'a> {
        DecimalIter::new(self)
    }
}

impl<Ptr: Borrow<Option<i128>>> FromIterator<Ptr> for DecimalArray {
    fn from_iter<I: IntoIterator<Item = Ptr>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let (lower, upper) = iter.size_hint();
        let size_hint = upper.unwrap_or(lower);

        let mut null_buf = BooleanBufferBuilder::new(size_hint);

        let buffer: Buffer = iter
            .map(|item| {
                if let Some(a) = item.borrow() {
                    null_buf.append(true);
                    *a
                } else {
                    null_buf.append(false);
                    // arbitrary value for NULL
                    0
                }
            })
            .collect();

        let data = unsafe {
            ArrayData::new_unchecked(
                Self::default_type(),
                null_buf.len(),
                None,
                Some(null_buf.into()),
                0,
                vec![buffer],
                vec![],
            )
        };
        DecimalArray::from(data)
    }
}

impl fmt::Debug for DecimalArray {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "DecimalArray<{}, {}>\n[\n", self.precision, self.scale)?;
        print_long_array(self, f, |array, index, f| {
            let formatted_decimal = array.value_as_string(index);

            write!(f, "{}", formatted_decimal)
        })?;
        write!(f, "]")
    }
}

impl Array for DecimalArray {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data(&self) -> &ArrayData {
        &self.data
    }

    fn into_data(self) -> ArrayData {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use crate::{array::DecimalBuilder, datatypes::Field};

    use super::*;

    #[test]
    fn test_decimal_array() {
        // let val_8887: [u8; 16] = [192, 219, 180, 17, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        // let val_neg_8887: [u8; 16] = [64, 36, 75, 238, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255];
        let values: [u8; 32] = [
            192, 219, 180, 17, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64, 36, 75, 238, 253,
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ];
        let array_data = ArrayData::builder(DataType::Decimal(38, 6))
            .len(2)
            .add_buffer(Buffer::from(&values[..]))
            .build()
            .unwrap();
        let decimal_array = DecimalArray::from(array_data);
        assert_eq!(8_887_000_000_i128, decimal_array.value(0).into());
        assert_eq!(-8_887_000_000_i128, decimal_array.value(1).into());
        assert_eq!(16, decimal_array.value_length());
    }

    #[test]
    #[cfg(not(feature = "force_validate"))]
    fn test_decimal_append_error_value() {
        let mut decimal_builder = DecimalBuilder::new(10, 5, 3);
        let mut result = decimal_builder.append_value(123456);
        let mut error = result.unwrap_err();
        assert_eq!(
            "Invalid argument error: 123456 is too large to store in a Decimal of precision 5. Max is 99999",
            error.to_string()
        );

        unsafe {
            decimal_builder.disable_value_validation();
        }
        result = decimal_builder.append_value(123456);
        assert!(result.is_ok());
        decimal_builder.append_value(12345).unwrap();
        let arr = decimal_builder.finish();
        assert_eq!("12.345", arr.value_as_string(1));

        decimal_builder = DecimalBuilder::new(10, 2, 1);
        result = decimal_builder.append_value(100);
        error = result.unwrap_err();
        assert_eq!(
            "Invalid argument error: 100 is too large to store in a Decimal of precision 2. Max is 99",
            error.to_string()
        );

        unsafe {
            decimal_builder.disable_value_validation();
        }
        result = decimal_builder.append_value(100);
        assert!(result.is_ok());
        decimal_builder.append_value(99).unwrap();
        result = decimal_builder.append_value(-100);
        assert!(result.is_ok());
        decimal_builder.append_value(-99).unwrap();
        let arr = decimal_builder.finish();
        assert_eq!("9.9", arr.value_as_string(1));
        assert_eq!("-9.9", arr.value_as_string(3));
    }

    #[test]
    fn test_decimal_from_iter_values() {
        let array = DecimalArray::from_iter_values(vec![-100, 0, 101].into_iter());
        assert_eq!(array.len(), 3);
        assert_eq!(array.data_type(), &DataType::Decimal(38, 10));
        assert_eq!(-100_i128, array.value(0).into());
        assert!(!array.is_null(0));
        assert_eq!(0_i128, array.value(1).into());
        assert!(!array.is_null(1));
        assert_eq!(101_i128, array.value(2).into());
        assert!(!array.is_null(2));
    }

    #[test]
    fn test_decimal_from_iter() {
        let array: DecimalArray = vec![Some(-100), None, Some(101)].into_iter().collect();
        assert_eq!(array.len(), 3);
        assert_eq!(array.data_type(), &DataType::Decimal(38, 10));
        assert_eq!(-100_i128, array.value(0).into());
        assert!(!array.is_null(0));
        assert!(array.is_null(1));
        assert_eq!(101_i128, array.value(2).into());
        assert!(!array.is_null(2));
    }

    #[test]
    fn test_decimal_iter() {
        let data = vec![Some(-100), None, Some(101)];
        let array: DecimalArray = data.clone().into_iter().collect();

        let collected: Vec<_> = array.iter().collect();
        assert_eq!(data, collected);
    }

    #[test]
    fn test_decimal_into_iter() {
        let data = vec![Some(-100), None, Some(101)];
        let array: DecimalArray = data.clone().into_iter().collect();

        let collected: Vec<_> = array.into_iter().collect();
        assert_eq!(data, collected);
    }

    #[test]
    fn test_decimal_iter_sized() {
        let data = vec![Some(-100), None, Some(101)];
        let array: DecimalArray = data.into_iter().collect();
        let mut iter = array.into_iter();

        // is exact sized
        assert_eq!(array.len(), 3);

        // size_hint is reported correctly
        assert_eq!(iter.size_hint(), (3, Some(3)));
        iter.next().unwrap();
        assert_eq!(iter.size_hint(), (2, Some(2)));
        iter.next().unwrap();
        iter.next().unwrap();
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert!(iter.next().is_none());
        assert_eq!(iter.size_hint(), (0, Some(0)));
    }

    #[test]
    fn test_decimal_array_value_as_string() {
        let arr = [123450, -123450, 100, -100, 10, -10, 0]
            .into_iter()
            .map(Some)
            .collect::<DecimalArray>()
            .with_precision_and_scale(6, 3)
            .unwrap();

        assert_eq!("123.450", arr.value_as_string(0));
        assert_eq!("-123.450", arr.value_as_string(1));
        assert_eq!("0.100", arr.value_as_string(2));
        assert_eq!("-0.100", arr.value_as_string(3));
        assert_eq!("0.010", arr.value_as_string(4));
        assert_eq!("-0.010", arr.value_as_string(5));
        assert_eq!("0.000", arr.value_as_string(6));
    }

    #[test]
    fn test_decimal_array_with_precision_and_scale() {
        let arr = DecimalArray::from_iter_values([12345, 456, 7890, -123223423432432])
            .with_precision_and_scale(20, 2)
            .unwrap();

        assert_eq!(arr.data_type(), &DataType::Decimal(20, 2));
        assert_eq!(arr.precision(), 20);
        assert_eq!(arr.scale(), 2);

        let actual: Vec<_> = (0..arr.len()).map(|i| arr.value_as_string(i)).collect();
        let expected = vec!["123.45", "4.56", "78.90", "-1232234234324.32"];

        assert_eq!(actual, expected);
    }

    #[test]
    #[should_panic(
        expected = "-123223423432432 is too small to store in a Decimal of precision 5. Min is -99999"
    )]
    fn test_decimal_array_with_precision_and_scale_out_of_range() {
        DecimalArray::from_iter_values([12345, 456, 7890, -123223423432432])
            // precision is too small to hold value
            .with_precision_and_scale(5, 2)
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "precision 40 is greater than max 38")]
    fn test_decimal_array_with_precision_and_scale_invalid_precision() {
        DecimalArray::from_iter_values([12345, 456])
            .with_precision_and_scale(40, 2)
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "scale 40 is greater than max 38")]
    fn test_decimal_array_with_precision_and_scale_invalid_scale() {
        DecimalArray::from_iter_values([12345, 456])
            .with_precision_and_scale(20, 40)
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "scale 10 is greater than precision 4")]
    fn test_decimal_array_with_precision_and_scale_invalid_precision_and_scale() {
        DecimalArray::from_iter_values([12345, 456])
            .with_precision_and_scale(4, 10)
            .unwrap();
    }

    #[test]
    fn test_decimal_array_fmt_debug() {
        let arr = [Some(8887000000), Some(-8887000000), None]
            .iter()
            .collect::<DecimalArray>()
            .with_precision_and_scale(23, 6)
            .unwrap();

        assert_eq!(
            "DecimalArray<23, 6>\n[\n  8887.000000,\n  -8887.000000,\n  null,\n]",
            format!("{:?}", arr)
        );
    }

    #[test]
    fn test_decimal_array_from_fixed_size_list() {
        let value_data = ArrayData::builder(DataType::UInt8)
            .offset(16)
            .len(48)
            .add_buffer(Buffer::from_slice_ref(&[99999_i128, 12, 34, 56]))
            .build()
            .unwrap();

        let null_buffer = Buffer::from_slice_ref(&[0b101]);

        // Construct a list array from the above two
        let list_data_type = DataType::FixedSizeList(
            Box::new(Field::new("item", DataType::UInt8, false)),
            16,
        );
        let list_data = ArrayData::builder(list_data_type)
            .len(2)
            .null_bit_buffer(Some(null_buffer))
            .offset(1)
            .add_child_data(value_data)
            .build()
            .unwrap();
        let list_array = FixedSizeListArray::from(list_data);
        let decimal = DecimalArray::from_fixed_size_list_array(list_array, 38, 0);

        assert_eq!(decimal.len(), 2);
        assert!(decimal.is_null(0));
        assert_eq!(decimal.value_as_string(1), "56".to_string());
    }
}
